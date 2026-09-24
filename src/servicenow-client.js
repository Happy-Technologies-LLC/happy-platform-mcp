/**
 * Happy MCP Server - REST API Client
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 */

import axios from 'axios';
import { userInfo } from 'node:os';
import { performAuthorizationCodeFlow } from './oauth-authorization-code.js';
import { createDefaultTokenStore } from './token-store.js';
import { InstanceCredentialStore, parseCredentialRef } from './instance-credential-store.js';

const CREDENTIAL_ERROR_CODES = new Set([
  'CREDENTIAL_NOT_FOUND',
  'INVALID_CREDENTIAL_REF',
  'KEYCHAIN_UNAVAILABLE',
  'KEYCHAIN_OPERATION_FAILED'
]);

function hasLegacySecret(value) {
  return value !== undefined && value !== null;
}

function credentialRefForType(ref, type, expectsObject = false) {
  const expectedType = type === 'password' ? 'password' : 'client-secret';
  if (expectsObject !== (ref && typeof ref === 'object' && !Array.isArray(ref))) {
    return undefined;
  }
  let candidate = ref;
  if (expectsObject) {
    const keys = Object.keys(ref);
    if (keys.some(key => !['password', 'clientSecret', 'passwordRef', 'clientSecretRef'].includes(key))) {
      return undefined;
    }
    candidate = type === 'password'
      ? (ref.password ?? ref.passwordRef)
      : (ref.clientSecret ?? ref.clientSecretRef);
  }
  if (typeof candidate !== 'string') return undefined;
  try {
    const parsed = parseCredentialRef(candidate);
    return parsed.type === expectedType ? parsed.ref : undefined;
  } catch {
    return undefined;
  }
}
function hasValidCredentialRefShape(ref, authType, grantType) {
  if (authType === 'oauth' && grantType === 'password') {
    return Boolean(
      credentialRefForType(ref, 'password', true)
      && credentialRefForType(ref, 'clientSecret', true)
    );
  }
  return Boolean(credentialRefForType(ref, authType === 'basic' ? 'password' : 'clientSecret', false));
}

function safeCredentialError(error) {
  const code = CREDENTIAL_ERROR_CODES.has(error?.code)
    ? error.code
    : 'KEYCHAIN_OPERATION_FAILED';
  const message = code === 'CREDENTIAL_NOT_FOUND'
    ? 'Credential not found'
    : code === 'INVALID_CREDENTIAL_REF'
      ? 'Invalid credential reference'
      : code === 'KEYCHAIN_UNAVAILABLE'
        ? 'Credential store unavailable'
        : 'Credential store operation failed';
  const safe = new Error(message);
  safe.code = code;
  safe.name = 'CredentialResolutionError';
  return safe;
}

export class StaleInstanceError extends Error {
  static code = 'INSTANCE_STALE_DURING_REQUEST';

  constructor() {
    super('Instance changed during credential resolution');
    this.name = 'StaleInstanceError';
    this.code = StaleInstanceError.code;
  }

  toJSON() {
    return {
      name: this.name,
      message: this.message,
      code: this.code
    };
  }
}
export class AuthenticationError extends Error {
  constructor(message, code = 'AUTHENTICATION_FAILED') {
    super(message);
    this.name = 'AuthenticationError';
    this.code = code;
  }

  toJSON() {
    return {
      name: this.name,
      message: this.message,
      code: this.code
    };
  }
}

function invalidTokenResponse(message) {
  return new AuthenticationError(message, 'OAUTH_TOKEN_RESPONSE_INVALID');
}

function validateTokenResponse(data) {
  if (!data || typeof data !== 'object' || Array.isArray(data)) {
    throw invalidTokenResponse('OAuth token endpoint returned an invalid response');
  }
  if (typeof data.access_token !== 'string' || data.access_token.trim().length === 0) {
    throw invalidTokenResponse('OAuth token endpoint response is missing a valid access token');
  }
  if (
    data.refresh_token !== undefined
    && (typeof data.refresh_token !== 'string' || data.refresh_token.trim().length === 0)
  ) {
    throw invalidTokenResponse('OAuth token endpoint response contains an invalid refresh token');
  }
  if (
    data.expires_in !== undefined
    && (
      typeof data.expires_in !== 'number'
      || !Number.isFinite(data.expires_in)
      || data.expires_in <= 0
      || data.expires_in > Number.MAX_SAFE_INTEGER / 1000
    )
  ) {
    throw invalidTokenResponse('OAuth token endpoint response contains an invalid expiry');
  }
  return data;
}

function hasValidCachedToken(state) {
  return (
    state?.generation !== undefined
    && typeof state.token === 'string'
    && state.token.length > 0
    && Number.isFinite(state.expiry)
    && Date.now() < state.expiry - 30000
  );
}

function staleInstanceError() {
  return new StaleInstanceError();
}

function isStaleInstanceError(error) {
  return error instanceof StaleInstanceError || error?.code === StaleInstanceError.code;
}

function collectCredentialValues(client) {
  const values = new Set();
  const add = (value) => {
    if (typeof value === 'string' && value.length > 0) {
      values.add(value);
    }
  };
  const addAuthorizationVariants = (scheme, value) => {
    if (typeof value !== 'string' || value.length === 0) return;
    for (const schemeVariant of [scheme, scheme.toLowerCase()]) {
      add(`${schemeVariant} ${value}`);
      add(`Authorization: ${schemeVariant} ${value}`);
      add(`authorization: ${schemeVariant} ${value}`);
    }
  };

  const password = client?.password;
  const username = client?.username;
  const clientSecret = client?.oauthConfig?.clientSecret;
  const basicAuth = client?.auth;
  const accessToken = client?.oauthToken;
  const refreshToken = client?.oauthRefreshToken;

  add(password);
  add(clientSecret);
  add(basicAuth);
  add(accessToken);
  add(refreshToken);
  if (typeof username === 'string' && typeof password === 'string') {
    const basicMaterial = `${username}:${password}`;
    const encodedBasic = Buffer.from(basicMaterial).toString('base64');
    add(basicMaterial);
    add(encodedBasic);
    addAuthorizationVariants('Basic', encodedBasic);
  }
  addAuthorizationVariants('Basic', basicAuth);
  addAuthorizationVariants('Bearer', accessToken);

  if (client?._resolvedCredentialSecrets) {
    for (const value of client._resolvedCredentialSecrets.values()) {
      add(value);
    }
  }
  if (client?._requestCredentialValues) {
    for (const value of client._requestCredentialValues) {
      add(value);
    }
  }
  return values;
}

function addAuthorizationValue(values, value) {
  if (typeof value !== 'string' || value.length === 0) return;
  values.add(value);
  const match = value.match(/^(?:authorization\s*:\s*)?(basic|bearer)\s+(.+)$/i);
  if (match) {
    values.add(match[2]);
    values.add(`${match[1]} ${match[2]}`);
    values.add(`${match[1].toLowerCase()} ${match[2]}`);
  }
}

function collectAuthorizationValues(value, values, seen = new Set(), key = '') {
  if (typeof value === 'string') {
    if (/authorization|_header/i.test(key)) {
      addAuthorizationValue(values, value);
    }
    return;
  }
  if (!value || typeof value !== 'object' || seen.has(value)) return;
  seen.add(value);
  for (const [childKey, childValue] of Object.entries(value)) {
    collectAuthorizationValues(childValue, values, seen, childKey);
  }
}

function collectErrorCredentialValues(value, values, seen = new Set(), key = '') {
  if (typeof value === 'string') {
    if (isCredentialField(key)) {
      addAuthorizationValue(values, value);
      values.add(value);
    } else if (/^(?:data|body)$/i.test(key)) {
      try {
        collectErrorCredentialValues(JSON.parse(value), values, seen, key);
      } catch {
        for (const [field, fieldValue] of new URLSearchParams(value)) {
          collectErrorCredentialValues(fieldValue, values, seen, field);
        }
      }
    }
    return;
  }
  if (!value || typeof value !== 'object' || seen.has(value)) return;
  seen.add(value);
  for (const [childKey, childValue] of Object.entries(value)) {
    collectErrorCredentialValues(childValue, values, seen, childKey);
  }
}

function isCredentialField(key) {
  return /^(?:authorization|access_token|refresh_token|client_secret|password|token|id_token|basic)$/i.test(key)
    || /authorization/i.test(key);
}

function redactCredentialStrings(value, values) {
  if (typeof value !== 'string') return value;
  return [...values]
    .sort((a, b) => b.length - a.length)
    .reduce((result, secret) => result.split(secret).join('[redacted]'), value);
}

function isRedactedField(key) {
  return isCredentialField(key) || /^headers?$/i.test(key);
}

function sanitizeCopy(value, values, seen = new Set()) {
  if (typeof value === 'string') return redactCredentialStrings(value, values);
  if (!value || typeof value !== 'object') return value;
  if (seen.has(value)) return '[Circular]';
  seen.add(value);
  const copy = Array.isArray(value) ? [] : {};
  for (const key of Object.keys(value)) {
    if (isRedactedField(key)) continue;
    let childValue;
    try {
      childValue = value[key];
    } catch {
      continue;
    }
    copy[key] = sanitizeCopy(childValue, values, seen);
  }
  return copy;
}

function isExplicitInvalidGrant(error) {
  const data = error?.response?.data;
  const candidates = [
    data,
    data?.error,
    data?.error?.code,
    data?.error_code,
    data?.code,
    error?.response?.error,
    error?.error,
    error?.code
  ];
  return candidates.some(value => typeof value === 'string' && value.toLowerCase() === 'invalid_grant');
}

function safeAuthError(error, client) {
  const values = collectCredentialValues(client);
  collectErrorCredentialValues(error, values);
  collectAuthorizationValues(error, values);
  const message = redactCredentialStrings(typeof error?.message === 'string' && error.message
    ? error.message
    : 'Authentication request failed', values);
  const safe = error instanceof AuthenticationError
    ? new AuthenticationError(message, error.code)
    : new Error(message);
  const status = error?.response?.status;
  if (status !== undefined) safe.status = status;
  if (status === 401) safe.code = 'AUTHENTICATION_FAILED';
  else if (status === 403) safe.code = 'AUTHORIZATION_FAILED';
  else if (typeof error?.code === 'string' && !CREDENTIAL_ERROR_CODES.has(error.code)) safe.code = error.code;
  for (const field of ['details', 'logs', 'config', 'request', 'response', 'payload']) {
    if (error?.[field] !== undefined) safe[field] = sanitizeCopy(error[field], values);
  }
  if (typeof error?.stack === 'string') {
    safe.stack = redactCredentialStrings(error.stack, values);
  }
  return safe;
}

function sanitizeServiceNowError(error, credentialSource, messageOverride) {
  const values = collectCredentialValues(credentialSource);
  collectAuthorizationValues(error?.config, values);
  collectAuthorizationValues(error?.response?.config, values);
  collectAuthorizationValues(error?.request, values);

  let originalSerialized;
  if (typeof error?.toJSON === 'function') {
    try {
      originalSerialized = sanitizeCopy(error.toJSON(), values);
    } catch {
      originalSerialized = undefined;
    }
  }

  const originalMessage = typeof error?.message === 'string' && error.message
    ? error.message
    : 'ServiceNow API request failed';
  const safe = new Error(redactCredentialStrings(messageOverride || originalMessage, values));
  if (typeof error?.name === 'string') safe.name = redactCredentialStrings(error.name, values);
  if (typeof error?.code === 'string') safe.code = error.code;
  if (error?.response?.status !== undefined) safe.status = error.response.status;

  if (error?.config !== undefined) safe.config = sanitizeCopy(error.config, values);
  if (error?.request !== undefined) safe.request = sanitizeCopy(error.request, values);
  if (error?.response !== undefined) safe.response = sanitizeCopy(error.response, values);
  if (error?.payload !== undefined) safe.payload = sanitizeCopy(error.payload, values);
  if (typeof error?.stack === 'string') {
    safe.stack = redactCredentialStrings(error.stack, values);
  }

  const toJSONSnapshot = () => {
    const snapshot = originalSerialized && typeof originalSerialized === 'object'
      ? { ...originalSerialized }
      : {};
    snapshot.name = safe.name;
    snapshot.message = safe.message;
    if (safe.code !== undefined) snapshot.code = safe.code;
    if (safe.status !== undefined) snapshot.status = safe.status;
    if (safe.stack !== undefined) snapshot.stack = safe.stack;
    if (safe.config !== undefined) snapshot.config = safe.config;
    if (safe.request !== undefined) snapshot.request = safe.request;
    if (safe.response !== undefined) snapshot.response = safe.response;
    if (safe.payload !== undefined) snapshot.payload = safe.payload;
    return sanitizeCopy(snapshot, values);
  };
  safe.toJSON = toJSONSnapshot;
  return safe;
}

function normalizeInstanceUrl(value) {
  return typeof value === 'string' ? value.replace(/\/+$/, '') : value;
}

function normalizePathname(pathname) {
  const normalized = pathname.replace(/\/+$/, '');
  return normalized || '/';
}

function requestBelongsToInstance(config, instanceUrl) {
  if (!config || typeof config !== 'object') return false;
  const capturedUrl = normalizeInstanceUrl(instanceUrl);
  if (normalizeInstanceUrl(config.baseURL) !== capturedUrl) return false;
  if (typeof config.url !== 'string' || config.url.length === 0) return false;

  let base;
  try {
    base = new URL(capturedUrl);
  } catch {
    return false;
  }

  const basePath = normalizePathname(base.pathname);
  const requestUrl = config.url;
  const absoluteRequest = /^[a-z][a-z\d+\-.]*:/i.test(requestUrl) || requestUrl.startsWith('//');
  let resolved;
  try {
    if (absoluteRequest) {
      resolved = new URL(requestUrl);
      if (resolved.origin !== base.origin) return false;
    } else {
      const pathEnd = requestUrl.search(/[?#]/);
      const rawPath = pathEnd === -1 ? requestUrl : requestUrl.slice(0, pathEnd);
      const suffix = pathEnd === -1 ? '' : requestUrl.slice(pathEnd);
      const relativePath = rawPath.replace(/^\/+/, '');
      const joinedPath = basePath === '/' ? `/${relativePath}` : `${basePath}/${relativePath}`;
      resolved = new URL(`${base.origin}${joinedPath}${suffix}`);
    }
  } catch {
    return false;
  }

  const resolvedPath = normalizePathname(resolved.pathname);
  return basePath === '/'
    ? true
    : resolvedPath === basePath || resolvedPath.startsWith(`${basePath}/`);
}

/** Default token-endpoint POST: form-encode params via axios, return the body. */
async function defaultTokenPost(url, params) {
  const response = await axios.post(url, new URLSearchParams(params).toString(), {
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' }
  });
  return response.data;
}

export function enrichServiceNowError(error, credentialSource = null) {
  const status = error?.response?.status;
  const data = error?.response?.data;
  const serviceNowError = data?.error;
  let message;

  if (serviceNowError?.message || serviceNowError?.detail) {
    message = [status, serviceNowError.message, serviceNowError.detail]
      .filter(Boolean)
      .join(' ');
  } else if (typeof data === 'string' && data.trim()) {
    message = [status, data.trim()].filter(Boolean).join(': ');
  }

  return sanitizeServiceNowError(error, credentialSource, message);
}

// Generates a unique per-execution marker so a background script's
// completion line can be found in syslog and never confused with output
// from an unrelated execution.
export function generateScriptExecutionMarker() {
  return `MCP_${Date.now()}_${Math.random().toString(16).slice(2, 10)}`;
}

// Matches ANY execution's start/end marker line (see
// buildBackgroundScriptWrapper), not just one specific run's — used by
// _collectBackgroundScriptLogs to strip marker bookkeeping out of
// returned log content regardless of which execution produced it.
const MCP_SCRIPT_MARKER_PATTERN = /^MCP_\d+_[0-9a-f]+:(START|END)/;

/**
 * Builds the sys_trigger script body that wraps a caller's script so its
 * outcome — success or a thrown error — is captured and reported via a
 * greppable `gs.info` marker line (see issue #40). The original wrapper was
 * a bare `try { script } finally { self-delete }` with no `catch`, so a
 * thrown script was silently indistinguishable from a clean one; the
 * `catch` here is what fixes that. `finally` still self-deletes the
 * trigger when `autoDelete` is set, using the trigger's own sys_id.
 *
 * A `:START` marker is logged before the script runs and a `:END` marker
 * (carrying the outcome JSON) after, bracketing a `sys_created_on` window
 * that `_collectBackgroundScriptLogs` later queries to recover whatever
 * the script itself logged via `gs.info`/`gs.print` in between.
 */
export function buildBackgroundScriptWrapper({ script, marker, autoDelete, triggerSysId }) {
  const startMarker = `${marker}:START`;
  const endMarker = `${marker}:END`;
  const deleteBlock = autoDelete
    ? `
  var triggerGR = new GlideRecord('sys_trigger');
  if (triggerGR.get('${triggerSysId}')) {
    triggerGR.deleteRecord();
  }`
    : '';

  return `
// Auto-generated MCP script trigger
gs.info('${startMarker}');
try {
  ${script}
  gs.info('${endMarker}' + JSON.stringify({ ok: true }));
} catch (e) {
  gs.info('${endMarker}' + JSON.stringify({ ok: false, error: e.message, stack: e.stack || '' }));
} finally {${deleteBlock}
}`;
}

/**
 * Parses a syslog record produced by `buildBackgroundScriptWrapper` into a
 * `{ state: 'completed' | 'failed', output?, error?, stack? }` outcome.
 */
export function parseBackgroundScriptMarker(record, marker) {
  const message = record?.message || '';
  const idx = message.indexOf(marker);
  const jsonPart = idx >= 0 ? message.slice(idx + marker.length) : message;

  let payload;
  try {
    payload = JSON.parse(jsonPart);
  } catch (parseError) {
    return {
      state: 'failed',
      error: `Could not parse script outcome from syslog message: ${message}`
    };
  }

  if (payload.ok) {
    const { ok, ...output } = payload;
    return { state: 'completed', output };
  }

  return { state: 'failed', error: payload.error || 'Script execution failed', stack: payload.stack };
}

export class ServiceNowClient {
  constructor(instanceUrl, username, password, options = {}) {
    this.currentInstanceName = 'default';
    this.oauthToken = null;
    this.oauthRefreshToken = null;
    this.oauthTokenExpiry = null;
    this.oauthConfig = null;
    this.setInstance(instanceUrl, username, password, null, options);
    this.progressCallback = null; // Callback for progress notifications
  }

  /**
   * Set progress callback for notifications
   * @param {Function} callback - Function to call with progress updates
   */
  setProgressCallback(callback) {
    this.progressCallback = callback;
  }

  /**
   * Send progress notification
   * @param {string} message - Progress message
   * @param {number} [current] - Current progress value
   * @param {number} [total] - Total value for the progress operation
   */
  notifyProgress(message, current, total) {
    if (this.progressCallback) {
      this.progressCallback(message, current, total);
    }
  }

  /**
   * Switch to a different ServiceNow instance
   * @param {string} instanceUrl - Instance URL
   * @param {string} username - Username
   * @param {string} password - Password
   * @param {string} instanceName - Optional instance name for tracking
   * @param {object} options - Optional config (authType, clientId, clientSecret)
   */
  setInstance(instanceUrl, username, password, instanceName = null, options = {}) {
    this._instanceGeneration = (this._instanceGeneration || 0) + 1;
    this._oauthCacheGeneration = this._instanceGeneration;
    this._resolvedCredentialSecrets = new Map();
    this._credentialResolutionPromises = new Map();
    this._oauthTokenPromise = null;
    this._credentialStore = options.credentialStore || null;
    this.credentialRef = options.credentialRef;
    this.instanceUrl = instanceUrl.replace(/\/$/, ''); // Remove trailing slash
    this.username = username;
    this.password = password;
    this.authType = options.authType || 'basic';

    if (instanceName) {
      this.currentInstanceName = instanceName;
    }

    if (this.authType === 'oauth') {
      this.oauthConfig = {
        clientId: options.clientId,
        clientSecret: options.clientSecret,
        grantType: options.grantType,
        scope: options.scope,
        // authorization_code (per-user) config — all optional, sensible defaults below
        authorizeUrl: options.authorizeUrl,
        tokenUrl: options.tokenUrl,
        redirectPort: options.redirectPort,
        callbackPath: options.callbackPath
      };
      // Injectable seams (defaults for production; overridden in tests)
      this._tokenStore = options.tokenStore || createDefaultTokenStore();
      this._performAuthCodeFlow = options.performAuthCodeFlow || performAuthorizationCodeFlow;
      this._postToken = options.postToken || defaultTokenPost;
      // Clear any stale token so the next request triggers a fresh grant
      this.oauthToken = null;
      this.oauthRefreshToken = null;
      this.oauthTokenExpiry = null;
    } else {
      this.auth = hasLegacySecret(password)
        ? Buffer.from(`${username}:${password}`).toString('base64')
        : null;
      this.oauthConfig = null;
      this.oauthToken = null;
      this.oauthRefreshToken = null;
      this.oauthTokenExpiry = null;
    }

    this._createClient();
  }

  /**
   * Create (or recreate) the axios client with the current auth config.
   * For OAuth, attaches request/response interceptors for token lifecycle.
   */
  _createClient() {
    const captured = {
      generation: this._instanceGeneration,
      instanceUrl: this.instanceUrl,
      instanceName: this.currentInstanceName,
      authType: this.authType,
      credentialContext: {
        username: this.username,
        password: this.password,
        auth: this.auth,
        oauthConfig: this.oauthConfig ? { ...this.oauthConfig } : null,
        oauthToken: this.oauthToken,
        oauthRefreshToken: this.oauthRefreshToken,
        _resolvedCredentialSecrets: this._resolvedCredentialSecrets,
        _requestCredentialValues: new Set()
      }
    };
    const client = axios.create({
      baseURL: captured.instanceUrl,
      headers: {
        'Content-Type': 'application/json',
        'Accept': 'application/json'
      }
    });
    captured.client = client;
    this.client = client;

    // Request interceptor — attach only the captured generation's credential.
    client.interceptors.request.use(async (config) => {
      this._assertCapturedRequest(captured, config);
      const authHeader = await this.getAuthHeader(captured.generation);
      this._assertCapturedRequest(captured, config);
      addAuthorizationValue(captured.credentialContext._requestCredentialValues, authHeader);
      config.headers = config.headers || {};
      config.headers.Authorization = authHeader;
      return config;
    });

    // Response interceptor — refresh on 401 and retry once against this client only.
    client.interceptors.response.use(
      (response) => {
        this._assertCapturedRequest(captured, response?.config);
        return response;
      },
      async (error) => {
        const originalRequest = error?.config || error?.response?.config;
        if (originalRequest) {
          this._assertCapturedRequest(captured, originalRequest);
        }
        if (captured.authType === 'oauth' && error.response?.status === 401) {
          if (!originalRequest._oauthRetry) {
            originalRequest._oauthRetry = true;
            this._assertCapturedRequest(captured, originalRequest);
            this.oauthToken = null;
            this.oauthTokenExpiry = null;
            this._assertCapturedRequest(captured, originalRequest);
            const token = await this._getOAuthToken(captured.generation);
            this._assertCapturedRequest(captured, originalRequest);
            addAuthorizationValue(captured.credentialContext._requestCredentialValues, `Bearer ${token}`);
            originalRequest.headers = originalRequest.headers || {};
            originalRequest.headers.Authorization = `Bearer ${token}`;
            this._assertCapturedRequest(captured, originalRequest);
            return client.request(originalRequest);
          }
        }
        throw enrichServiceNowError(error, captured.credentialContext);
      }
    );
  }

  _assertCapturedRequest(captured, config) {
    this._assertCurrentGeneration(captured.generation);
    if (
      this.client !== captured.client
      || this.authType !== captured.authType
      || !requestBelongsToInstance(config, captured.instanceUrl)
    ) {
      throw staleInstanceError();
    }
  }

  _assertCurrentGeneration(generation) {
    if (generation !== this._instanceGeneration) {
      throw staleInstanceError();
    }
  }

  async _resolveCredential(type, generation = this._instanceGeneration) {
    this._assertCurrentGeneration(generation);
    const legacy = type === 'password' ? this.password : this.oauthConfig?.clientSecret;
    if (hasLegacySecret(legacy)) {
      return legacy;
    }

    const grantType = this.oauthConfig?.grantType || (this.username ? 'password' : 'client_credentials');
    const expectsObject = this.authType === 'oauth' && grantType === 'password';
    if (!hasValidCredentialRefShape(this.credentialRef, this.authType, grantType)) {
      throw safeCredentialError({ code: 'CREDENTIAL_NOT_FOUND' });
    }
    const ref = credentialRefForType(this.credentialRef, type, expectsObject);
    const key = `${type}:${ref}`;
    if (this._resolvedCredentialSecrets.has(key)) {
      return this._resolvedCredentialSecrets.get(key);
    }
    const promises = this._credentialResolutionPromises;
    if (!promises.has(key)) {
      let promise;
      promise = this._loadCredential(ref, key, generation).finally(() => {
        if (
          this._instanceGeneration === generation
          && this._credentialResolutionPromises === promises
          && promises.get(key) === promise
        ) {
          promises.delete(key);
        }
      });
      promises.set(key, promise);
    }
    return promises.get(key);
  }

  async _loadCredential(ref, key, generation) {
    const store = this._credentialStore || new InstanceCredentialStore();
    try {
      const value = await store.getSecret(ref);
      this._assertCurrentGeneration(generation);
      if (typeof value !== 'string' || value.length === 0) {
        throw { code: 'CREDENTIAL_NOT_FOUND' };
      }
      this._resolvedCredentialSecrets.set(key, value);
      if (!this._credentialStore) {
        this._credentialStore = store;
      }
      return value;
    } catch (error) {
      if (isStaleInstanceError(error)) {
        throw error;
      }
      throw safeCredentialError(error);
    }
  }

  /**
   * Get a valid OAuth access token, requesting or refreshing as needed.
   * Uses ServiceNow's /oauth_token.do endpoint.
   * @returns {Promise<string>} Access token
   */
  async _getOAuthToken(generation = this._instanceGeneration) {
    this._assertCurrentGeneration(generation);
    if (this.oauthToken && this.oauthTokenExpiry && Date.now() < this.oauthTokenExpiry - 30000) {
      this._assertCurrentGeneration(generation);
      return this.oauthToken;
    }
    if (this._oauthTokenPromise) {
      const token = await this._oauthTokenPromise;
      this._assertCurrentGeneration(generation);
      return token;
    }

    const promise = this._requestOAuthToken(generation);
    this._oauthTokenPromise = promise;
    try {
      this._assertCurrentGeneration(generation);
      const token = await promise;
      this._assertCurrentGeneration(generation);
      return token;
    } finally {
      if (this._oauthTokenPromise === promise) {
        this._oauthTokenPromise = null;
      }
    }
  }

  async _requestOAuthToken(generation) {
    // Per-user authorization_code clients are public and never resolve a static secret.
    if (this.oauthConfig?.grantType === 'authorization_code') {
      return this._getAuthorizationCodeToken(generation);
    }
    this._assertCurrentGeneration(generation);
    const clientSecret = await this._resolveCredential('clientSecret', generation);
    this._assertCurrentGeneration(generation);
    const tokenUrl = this.oauthConfig.tokenUrl || `${this.instanceUrl}/oauth_token.do`;

    // Try refresh token first if we have one
    if (this.oauthRefreshToken) {
      try {
        const response = await axios.post(tokenUrl, new URLSearchParams({
          grant_type: 'refresh_token',
          client_id: this.oauthConfig.clientId,
          client_secret: clientSecret,
          refresh_token: this.oauthRefreshToken
        }).toString(), {
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' }
        });

        this._assertCurrentGeneration(generation);
        return this._handleTokenResponse(response.data, generation);
      } catch (refreshError) {
        this._assertCurrentGeneration(generation);
        if (isStaleInstanceError(refreshError)) {
          throw refreshError;
        }
        if (!isExplicitInvalidGrant(refreshError)) {
          const safe = safeAuthError(refreshError, this);
          if (!safe.code) safe.code = 'OAUTH_REFRESH_REQUEST_FAILED';
          throw safe;
        }
        // An explicit invalid_grant means the refresh token is dead. Clear it
        // in memory, then intentionally try the configured primary grant.
        console.error('OAuth refresh token rejected; requesting the configured primary grant');
        this._assertCurrentGeneration(generation);
        this.oauthRefreshToken = null;
        this._assertCurrentGeneration(generation);
      }
    }

    // Determine grant type: client_credentials if no username/password, otherwise password (ROPC)
    const grantType = this.oauthConfig.grantType || (this.username ? 'password' : 'client_credentials');
    const params = {
      grant_type: grantType,
      client_id: this.oauthConfig.clientId,
      client_secret: clientSecret
    };

    if (grantType === 'password') {
      params.username = this.username;
      params.password = await this._resolveCredential('password', generation);
    }

    if (this.oauthConfig.scope) {
      params.scope = this.oauthConfig.scope;
    }

    try {
      this._assertCurrentGeneration(generation);
      const response = await axios.post(tokenUrl, new URLSearchParams(params).toString(), {
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' }
      });

      this._assertCurrentGeneration(generation);
      return this._handleTokenResponse(response.data, generation);
    } catch (error) {
      if (isStaleInstanceError(error)) {
        throw error;
      }
      const safe = safeAuthError(error, this);
      if (!safe.code) safe.code = 'OAUTH_TOKEN_REQUEST_FAILED';
      throw safe;
    }
  }

  /**
   * Per-user authorization_code token lifecycle.
   *
   * Order: valid cached token → refresh from the persisted refresh token →
   * interactive browser sign-in. A rejected refresh token FAILS LOUD: it never
   * falls back to a password/client_credentials grant (that would re-introduce
   * shared-principal attribution). Instead it re-prompts the browser sign-in.
   * Refresh tokens are persisted per local OS user and instance in the injected token store.
   * @returns {Promise<string>} Access token
   */
  async _getAuthorizationCodeToken(generation = this._instanceGeneration) {
    this._assertCurrentGeneration(generation);
    // Return cached token if still valid (with 30s buffer)
    if (this.oauthToken && this.oauthTokenExpiry && Date.now() < this.oauthTokenExpiry - 30000) {
      return this.oauthToken;
    }

    const tokenStore = this._tokenStore;
    const oauthConfig = { ...this.oauthConfig };
    const account = `${userInfo().username}@${this.currentInstanceName}`;
    const tokenUrl = oauthConfig.tokenUrl || `${this.instanceUrl}/oauth_token.do`;
    const authorizeUrl = oauthConfig.authorizeUrl || `${this.instanceUrl}/oauth_auth.do`;

    // Load a persisted refresh token if we don't have one in memory yet.
    if (!this.oauthRefreshToken) {
      try {
        this._assertCurrentGeneration(generation);
        const storedRefreshToken = await tokenStore.getRefreshToken(account);
        this._assertCurrentGeneration(generation);
        this.oauthRefreshToken = storedRefreshToken;
      } catch (error) {
        if (isStaleInstanceError(error)) {
          throw error;
        }
        throw safeCredentialError(error);
      }
    }

    // Try the refresh token first — avoids a browser round-trip.
    if (this.oauthRefreshToken) {
      try {
        const params = {
          grant_type: 'refresh_token',
          client_id: oauthConfig.clientId,
          refresh_token: this.oauthRefreshToken
        };
        if (oauthConfig.clientSecret) {
          params.client_secret = oauthConfig.clientSecret;
        }
        this._assertCurrentGeneration(generation);
        const data = await this._postToken(tokenUrl, params);
        this._assertCurrentGeneration(generation);
        return this._acceptAuthCodeTokens(data, account, generation, tokenStore);
      } catch (refreshError) {
        this._assertCurrentGeneration(generation);

        if (isStaleInstanceError(refreshError)) {
          throw refreshError;
        }
        // Only an explicit OAuth invalid_grant means the refresh token is dead.
        // Other 400/401 responses (for example invalid_client or invalid_request)
        // must preserve the token and surface the failure without re-auth.
        const status = refreshError.response?.status;
        if (
          (status !== 400 && status !== 401)
          || !isExplicitInvalidGrant(refreshError)
        ) {
          throw safeAuthError(refreshError, this);
        }
        // FAIL LOUD, but never fall back to password: re-prompt sign-in.
        console.error('OAuth refresh token rejected; re-authenticating via browser sign-in');
        this._assertCurrentGeneration(generation);
        this.oauthRefreshToken = null;
        this._assertCurrentGeneration(generation);
        try {
          await tokenStore.clearRefreshToken(account);
          this._assertCurrentGeneration(generation);
        } catch (error) {
          if (isStaleInstanceError(error)) {
            throw error;
          }
          throw safeCredentialError(error);
        }
      }
    }

    // Interactive authorization_code + PKCE + loopback sign-in.
    let data;
    try {
      this._assertCurrentGeneration(generation);
      data = await this._performAuthCodeFlow({
        authorizeUrl,
        tokenUrl,
        clientId: oauthConfig.clientId,
        clientSecret: oauthConfig.clientSecret,
        scope: oauthConfig.scope,
        redirectPort: oauthConfig.redirectPort,
        callbackPath: oauthConfig.callbackPath
      });
      this._assertCurrentGeneration(generation);
    } catch (error) {
      this._assertCurrentGeneration(generation);

      if (isStaleInstanceError(error)) {
        throw error;
      }
      throw safeAuthError(error, this);
    }
    return this._acceptAuthCodeTokens(data, account, generation, tokenStore);
  }

  /**
   * Cache an authorization_code token response and persist the refresh token.
   * @param {object} data - Token endpoint response
   * @param {string} account - Token-store account key
   * @returns {Promise<string>} Access token
   */
  async _acceptAuthCodeTokens(data, account, generation, tokenStore) {
    this._assertCurrentGeneration(generation);
    const validated = validateTokenResponse(data);
    const previous = {
      generation: this._oauthCacheGeneration,
      token: this.oauthToken,
      refreshToken: this.oauthRefreshToken,
      expiry: this.oauthTokenExpiry
    };

    // Persist only when the response carries a refresh token that differs from
    // the stored value. Do not publish any response-derived cache state until
    // this write succeeds, because an access token without its refresh token
    // would be unrecoverable on the next refresh.
    try {
      if (validated.refresh_token !== undefined) {
        this._assertCurrentGeneration(generation);
        const stored = await tokenStore.getRefreshToken(account);
        this._assertCurrentGeneration(generation);
        if (validated.refresh_token !== stored) {
          this._assertCurrentGeneration(generation);
          await tokenStore.setRefreshToken(account, validated.refresh_token);
          this._assertCurrentGeneration(generation);
        }
      }
    } catch (error) {
      if (isStaleInstanceError(error)) {
        throw error;
      }
      this._assertCurrentGeneration(generation);
      if (previous.generation === generation && hasValidCachedToken(previous)) {
        this.oauthToken = previous.token;
        this.oauthRefreshToken = previous.refreshToken;
        this.oauthTokenExpiry = previous.expiry;
      } else {
        this.oauthToken = null;
        this.oauthRefreshToken = null;
        this.oauthTokenExpiry = null;
        this._oauthCacheGeneration = generation;
      }
      throw safeCredentialError(error);
    }

    this._assertCurrentGeneration(generation);
    this.oauthToken = validated.access_token;
    this._assertCurrentGeneration(generation);
    this.oauthRefreshToken = validated.refresh_token ?? this.oauthRefreshToken;
    this._assertCurrentGeneration(generation);
    this.oauthTokenExpiry = Date.now() + (validated.expires_in ?? 1800) * 1000;
    this._oauthCacheGeneration = generation;
    return this.oauthToken;
  }

  /**
   * Process an OAuth token response and cache the credentials.
   * @param {object} data - Token endpoint response
   * @returns {string} Access token
   */
  _handleTokenResponse(data, generation = this._instanceGeneration) {
    this._assertCurrentGeneration(generation);
    const validated = validateTokenResponse(data);
    const expiry = Date.now() + (validated.expires_in ?? 1800) * 1000;
    this._assertCurrentGeneration(generation);
    this.oauthToken = validated.access_token;
    this._assertCurrentGeneration(generation);
    this.oauthRefreshToken = validated.refresh_token ?? this.oauthRefreshToken;
    this._assertCurrentGeneration(generation);
    this.oauthTokenExpiry = expiry;
    this._oauthCacheGeneration = generation;
    return this.oauthToken;
  }

  /**
   * Get the current Authorization header value.
   * For OAuth, returns the cached token (does not refresh — use _getOAuthToken for that).
   * Useful for one-off axios instances that need the current auth.
   * @returns {Promise<string>} Authorization header value
   */
  async getAuthHeader(generation = this._instanceGeneration, authType = this.authType) {
    this._assertCurrentGeneration(generation);
    if (authType !== this.authType) {
      throw staleInstanceError();
    }
    if (authType === 'oauth') {
      const token = await this._getOAuthToken(generation);
      this._assertCurrentGeneration(generation);
      return `Bearer ${token}`;
    }
    if (hasLegacySecret(this.password)) {
      this._assertCurrentGeneration(generation);
      return `Basic ${this.auth}`;
    }
    const password = await this._resolveCredential('password', generation);
    this._assertCurrentGeneration(generation);
    return `Basic ${Buffer.from(`${this.username}:${password}`).toString('base64')}`;
  }

  /**
   * Get current instance information
   * @returns {object} Current instance details
   */
  getCurrentInstance() {
    return {
      name: this.currentInstanceName,
      url: this.instanceUrl,
      authType: this.authType || 'basic'
    };
  }

  // Generic table operations
  async getRecords(table, query = {}) {
    const params = new URLSearchParams();
    if (query.sysparm_query) params.append('sysparm_query', query.sysparm_query);
    if (query.sysparm_limit) params.append('sysparm_limit', query.sysparm_limit);
    if (query.sysparm_fields) params.append('sysparm_fields', query.sysparm_fields);
    if (query.sysparm_offset) params.append('sysparm_offset', query.sysparm_offset);
    if (query.sysparm_order_by) params.append('sysparm_order_by', query.sysparm_order_by);

    const response = await this.client.get(`/api/now/table/${table}?${params}`);
    return response.data.result;
  }

  async getRecord(table, sysId, queryParams = {}) {
    const params = new URLSearchParams();
    if (queryParams.sysparm_fields) params.append('sysparm_fields', queryParams.sysparm_fields);
    if (queryParams.sysparm_display_value) params.append('sysparm_display_value', queryParams.sysparm_display_value);
    if (queryParams.sysparm_exclude_reference_link) params.append('sysparm_exclude_reference_link', queryParams.sysparm_exclude_reference_link);

    const queryString = params.toString();
    const url = queryString ? `/api/now/table/${table}/${sysId}?${queryString}` : `/api/now/table/${table}/${sysId}`;

    const response = await this.client.get(url);
    return response.data.result;
  }

  async createRecord(table, data) {
    const response = await this.client.post(`/api/now/table/${table}`, data);
    return response.data.result;
  }

  async updateRecord(table, sysId, data) {
    const response = await this.client.put(`/api/now/table/${table}/${sysId}`, data);
    return response.data.result;
  }

  async deleteRecord(table, sysId) {
    await this.client.delete(`/api/now/table/${table}/${sysId}`);
    return { success: true };
  }

  // Update set management via UI API endpoint
  async setCurrentUpdateSet(updateSetSysId) {
    try {
      // First, get the update set name
      const updateSet = await this.getRecord('sys_update_set', updateSetSysId);

      // Create axios client with UI session
      const authHeader = await this.getAuthHeader();
      const axiosWithCookies = axios.create({
        baseURL: this.instanceUrl,
        headers: {
          'Authorization': authHeader,
          'Content-Type': 'application/json',
          'Accept': 'application/json',
          'User-Agent': 'ServiceNow-MCP-Client/2.0'
        },
        withCredentials: true,
        maxRedirects: 5
      });

      // Establish session first
      await axiosWithCookies.get('/', {
        headers: { 'Accept': 'text/html' }
      });

      // Set the update set via UI API
      const response = await axiosWithCookies.put(
        '/api/now/ui/concoursepicker/updateset',
        {
          name: updateSet.name,
          sysId: updateSetSysId
        }
      );

      return {
        success: true,
        update_set: updateSet.name,
        sys_id: updateSetSysId,
        response: response.data
      };
    } catch (error) {
      // If UI API fails, fall back to sys_trigger method
      console.log('UI API failed, falling back to sys_trigger...');

      const updateSet = await this.getRecord('sys_update_set', updateSetSysId);
      const script = `// Update user preference for current update set
var updateSetId = '${updateSetSysId}';

// Delete existing preference
var delGR = new GlideRecord('sys_user_preference');
delGR.addQuery('user', gs.getUserID());
delGR.addQuery('name', 'sys_update_set');
delGR.query();
if (delGR.next()) {
  delGR.deleteRecord();
}

// Create new preference
var gr = new GlideRecord('sys_user_preference');
gr.initialize();
gr.user = gs.getUserID();
gr.name = 'sys_update_set';
gr.value = updateSetId;
gr.insert();

gs.info('✅ Update set changed to: ${updateSet.name}');`;

      const result = await this.executeScriptViaTrigger(script, `Set update set to: ${updateSet.name}`, true, { wait: false });
      return {
        success: true,
        update_set: updateSet.name,
        sys_id: updateSetSysId,
        method: 'sys_trigger',
        trigger_details: result
      };
    }
  }

  async getCurrentUpdateSet() {
    // Get the current update set preference
    const response = await this.client.get(`/api/now/ui/preferences/sys_update_set`);
    return response.data;
  }

  async listUpdateSets(query = {}) {
    // List available update sets
    return this.getRecords('sys_update_set', query);
  }

  async setCurrentApplication(appSysId) {
    const startTime = Date.now();

    try {
      // Validate input
      if (!appSysId) {
        throw new Error('app_sys_id is required');
      }

      // Validate sys_id format (32-character hex string)
      if (!/^[0-9a-f]{32}$/i.test(appSysId)) {
        throw new Error(`Invalid sys_id format: ${appSysId}. Must be a 32-character hexadecimal string.`);
      }

      // Get previous application scope (for rollback information)
      let previousScope = null;
      try {
        const prefResponse = await this.client.get('/api/now/ui/preferences/apps.current');
        if (prefResponse.data && prefResponse.data.result) {
          previousScope = {
            sys_id: prefResponse.data.result.value || null,
            name: prefResponse.data.result.display_value || null
          };
        }
      } catch (prefError) {
        // Previous scope query failed - not critical, continue
        console.log('Could not retrieve previous scope:', prefError.message);
      }

      // Get application details
      let app;
      try {
        app = await this.getRecord('sys_app', appSysId);
      } catch (appError) {
        if (appError.response && appError.response.status === 404) {
          throw new Error(`Application not found with sys_id: ${appSysId}. Please verify the sys_id is correct.`);
        }
        throw appError;
      }

      // Create axios client with cookie jar
      const authHeader = await this.getAuthHeader();
      const axiosWithCookies = axios.create({
        baseURL: this.instanceUrl,
        headers: {
          'Authorization': authHeader,
          'Content-Type': 'application/json',
          'User-Agent': 'ServiceNow-MCP-Client/2.0'
        },
        withCredentials: true,
        maxRedirects: 5
      });

      // Establish session first
      await axiosWithCookies.get('/', {
        headers: { 'Accept': 'text/html' }
      });

      // Set the application via UI API
      const response = await axiosWithCookies.put(
        '/api/now/ui/concoursepicker/application',
        {
          app_id: appSysId
        }
      );

      // Verify the scope was set correctly
      let verified = false;
      let verificationError = null;
      try {
        await new Promise(resolve => setTimeout(resolve, 500)); // Wait for preference to update
        const verifyResponse = await this.client.get('/api/now/ui/preferences/apps.current');
        if (verifyResponse.data && verifyResponse.data.result) {
          const currentAppId = verifyResponse.data.result.value;
          verified = (currentAppId === appSysId);
          if (!verified) {
            verificationError = `Verification failed: Current app is ${currentAppId}, expected ${appSysId}`;
          }
        }
      } catch (verifyError) {
        verificationError = `Verification query failed: ${verifyError.message}`;
      }

      const executionTime = Date.now() - startTime;

      const result = {
        success: true,
        application: app.name,
        scope: app.scope || 'global',
        sys_id: appSysId,
        previous_scope: previousScope,
        verified: verified,
        verification_error: verificationError,
        timestamp: new Date().toISOString(),
        execution_time_ms: executionTime,
        method: 'ui_api',
        endpoint: '/api/now/ui/concoursepicker/application',
        response: response.data
      };

      // Add warnings if applicable
      result.warnings = [];
      if (!verified) {
        result.warnings.push('Could not verify scope change - please check ServiceNow UI');
      }
      if (verificationError) {
        result.warnings.push(verificationError);
      }

      return result;
    } catch (error) {
      const executionTime = Date.now() - startTime;

      // Enhanced error messages based on error type
      let errorMessage = error.message;

      if (error.response) {
        const status = error.response.status;
        if (status === 401) {
          errorMessage = 'Authentication failed. Please check your credentials.';
        } else if (status === 403) {
          errorMessage = `Access denied. Please verify:\n1. You have admin or developer role\n2. You have access to the application\n3. The application is active`;
        } else if (status === 404) {
          errorMessage = `Application not found with sys_id: ${appSysId}`;
        } else if (status >= 500) {
          errorMessage = `ServiceNow server error (${status}). Please try again later.`;
        }
      }

      console.error('Failed to set current application:', errorMessage);

      const enhancedError = new Error(`Failed to set current application: ${errorMessage}`);
      enhancedError.execution_time_ms = executionTime;
      enhancedError.app_sys_id = appSysId;
      enhancedError.original_error = error;

      throw enhancedError;
    }
  }

  // Incident-specific methods
  async getIncidents(query = {}) {
    return this.getRecords('incident', query);
  }

  async getIncidentByNumber(number) {
    const incidents = await this.getRecords('incident', {
      sysparm_query: `number=${number}`,
      sysparm_limit: 1
    });
    return incidents[0] || null;
  }

  async createIncident(data) {
    return this.createRecord('incident', data);
  }

  async updateIncident(sysId, data) {
    return this.updateRecord('incident', sysId, data);
  }

  // User management
  async getUsers(query = {}) {
    return this.getRecords('sys_user', query);
  }

  async createUser(data) {
    return this.createRecord('sys_user', data);
  }

  async updateUser(sysId, data) {
    return this.updateRecord('sys_user', sysId, data);
  }

  // Catalog items
  async getCatalogItems(query = {}) {
    return this.getRecords('sc_cat_item', query);
  }

  async createCatalogItem(data) {
    return this.createRecord('sc_cat_item', data);
  }

  // Change requests
  async getChangeRequests(query = {}) {
    return this.getRecords('change_request', query);
  }

  async getChangeRequestByNumber(number) {
    const changes = await this.getRecords('change_request', {
      sysparm_query: `number=${number}`,
      sysparm_limit: 1
    });
    return changes[0] || null;
  }

  async createChangeRequest(data) {
    return this.createRecord('change_request', data);
  }

  async updateChangeRequest(sysId, data) {
    return this.updateRecord('change_request', sysId, data);
  }

  // Problems
  async getProblems(query = {}) {
    return this.getRecords('problem', query);
  }

  async getProblemByNumber(number) {
    const problems = await this.getRecords('problem', {
      sysparm_query: `number=${number}`,
      sysparm_limit: 1
    });
    return problems[0] || null;
  }

  async createProblem(data) {
    return this.createRecord('problem', data);
  }

  async updateProblem(sysId, data) {
    return this.updateRecord('problem', sysId, data);
  }

  // Service Requests
  async getServiceRequests(query = {}) {
    return this.getRecords('sc_request', query);
  }

  async createServiceRequest(data) {
    return this.createRecord('sc_request', data);
  }

  async updateServiceRequest(sysId, data) {
    return this.updateRecord('sc_request', sysId, data);
  }

  // Groups
  async getGroups(query = {}) {
    return this.getRecords('sys_user_group', query);
  }

  async createGroup(data) {
    return this.createRecord('sys_user_group', data);
  }

  async updateGroup(sysId, data) {
    return this.updateRecord('sys_user_group', sysId, data);
  }

  // Group membership
  async addUserToGroup(userId, groupId) {
    return this.createRecord('sys_user_grmember', {
      user: userId,
      group: groupId
    });
  }

  async removeUserFromGroup(userId, groupId) {
    const members = await this.getRecords('sys_user_grmember', {
      sysparm_query: `user=${userId}^group=${groupId}`,
      sysparm_limit: 1
    });
    if (members[0]) {
      await this.deleteRecord('sys_user_grmember', members[0].sys_id);
    }
    return { success: true };
  }

  // Knowledge Base
  async getKnowledgeBases(query = {}) {
    return this.getRecords('kb_knowledge_base', query);
  }

  async createKnowledgeBase(data) {
    return this.createRecord('kb_knowledge_base', data);
  }

  // Knowledge Articles
  async getKnowledgeArticles(query = {}) {
    return this.getRecords('kb_knowledge', query);
  }

  async createKnowledgeArticle(data) {
    return this.createRecord('kb_knowledge', data);
  }

  async updateKnowledgeArticle(sysId, data) {
    return this.updateRecord('kb_knowledge', sysId, data);
  }

  // Catalog Categories
  async getCatalogCategories(query = {}) {
    return this.getRecords('sc_category', query);
  }

  async createCatalogCategory(data) {
    return this.createRecord('sc_category', data);
  }

  async updateCatalogCategory(sysId, data) {
    return this.updateRecord('sc_category', sysId, data);
  }

  // Service Catalog — AI-submission helpers

  // Maps ServiceNow variable type codes to human-readable names
  static VARIABLE_TYPE_MAP = {
    '1': 'single_line_text',
    '2': 'multi_line_text',
    '3': 'multiple_choice',
    '4': 'checkbox',
    '5': 'select_box',
    '6': 'reference',
    '7': 'date',
    '8': 'date_time',
    '9': 'label',
    '10': 'break',
    '11': 'macro',
    '12': 'ui_page',
    '14': 'wide_single_line_text',
    '15': 'masked',
    '16': 'container_start',
    '17': 'container_end',
    '18': 'list_collector',
    '19': 'lookup_select_box',
    '20': 'lookup_multiple_choice',
    '21': 'html',
    '22': 'split_attachments',
    '23': 'macro_with_label',
    '24': 'email',
    '25': 'url',
    '26': 'ip_address',
    '27': 'duration',
    '28': 'number_scale',
    '29': 'template',
    '30': 'custom_table'
  };

  /**
   * Retrieve full form context for a catalog item or record producer,
   * including enriched variables, choice options, variable sets,
   * UI policy effects, and client script names.
   * @param {string} sysId - Catalog item sys_id
   * @returns {object} Full form context object
   */
  async getCatalogItemDetail(sysId) {
    // Fetch the catalog item record
    const item = await this.getRecord('sc_cat_item', sysId, {
      sysparm_fields: 'sys_id,name,short_description,description,category,sc_catalogs,active,order,price,recurring_price,delivery_time,sys_class_name',
      sysparm_display_value: 'false'
    });

    // Fetch all active variables ordered by display order
    const variables = await this.getRecords('item_option_new', {
      sysparm_query: `cat_item=${sysId}^active=true^ORDERBYorder`,
      sysparm_fields: 'sys_id,name,question_text,type,mandatory,order,default_value,help_text,reference,reference_qual,cat_item,variable_set',
      sysparm_limit: 500
    });

    // Collect sys_ids of choice-bearing variable types
    const CHOICE_TYPES = new Set(['3', '5', '19', '20']);
    const choiceVarIds = variables
      .filter(v => CHOICE_TYPES.has(String(v.type)))
      .map(v => v.sys_id);

    // Parallel fetch: choices, variable set links, UI policies, client scripts
    const [allChoicesRaw, varSetLinks, uiPolicies, clientScripts] = await Promise.all([
      choiceVarIds.length > 0
        ? this.getRecords('question_choice', {
            sysparm_query: `question=IN${choiceVarIds.join(',')}^ORDERBYorder`,
            sysparm_fields: 'sys_id,question,text,value,order',
            sysparm_limit: 2000
          })
        : Promise.resolve([]),
      this.getRecords('io_set_item', {
        sysparm_query: `sc_cat_item=${sysId}`,
        sysparm_fields: 'sys_id,variable_set',
        sysparm_limit: 50
      }),
      this.getRecords('catalog_ui_policy', {
        sysparm_query: `catalog_item=${sysId}^active=true`,
        sysparm_fields: 'sys_id,short_description,conditions,run_scripts,reverse_if_false',
        sysparm_limit: 100
      }),
      this.getRecords('catalog_script_client', {
        sysparm_query: `cat_item=${sysId}^active=true`,
        sysparm_fields: 'sys_id,name,script_name,type,applies_to',
        sysparm_limit: 100
      })
    ]);

    // Index choices by variable sys_id
    const choicesByVar = {};
    allChoicesRaw.forEach(c => {
      const varId = typeof c.question === 'object' ? c.question.value : c.question;
      if (!choicesByVar[varId]) choicesByVar[varId] = [];
      choicesByVar[varId].push({ text: c.text, value: c.value });
    });

    // Fetch UI policy actions if there are policies
    let policyActions = [];
    if (uiPolicies.length > 0) {
      const policyIds = uiPolicies.map(p => p.sys_id);
      policyActions = await this.getRecords('catalog_ui_policy_action', {
        sysparm_query: `ui_policy=IN${policyIds.join(',')}`,
        sysparm_fields: 'sys_id,ui_policy,catalog_variable,mandatory,visible,read_only',
        sysparm_limit: 500
      });
    }

    // Build enriched variable list
    const enrichedVariables = variables.map(v => {
      const typeCode = String(v.type);
      const typeName = ServiceNowClient.VARIABLE_TYPE_MAP[typeCode] || `unknown_${typeCode}`;
      const varChoices = choicesByVar[v.sys_id];
      return {
        sys_id: v.sys_id,
        name: v.name,
        label: v.question_text,
        type: typeName,
        type_code: typeCode,
        mandatory: v.mandatory === 'true' || v.mandatory === true,
        order: parseInt(v.order, 10) || 0,
        default_value: v.default_value || null,
        help_text: v.help_text || null,
        reference_table: v.reference || null,
        ...(varChoices ? { choices: varChoices } : {})
      };
    });

    return {
      sys_id: item.sys_id,
      name: item.name,
      short_description: item.short_description,
      description: item.description,
      category: item.category,
      active: item.active,
      item_type: item.sys_class_name === 'sc_cat_item_producer' ? 'record_producer' : 'catalog_item',
      variables: enrichedVariables,
      variable_sets: varSetLinks.map(l => ({
        sys_id: typeof l.variable_set === 'object' ? l.variable_set.value : l.variable_set,
        display_value: typeof l.variable_set === 'object' ? l.variable_set.display_value : l.variable_set
      })),
      ui_policies: uiPolicies.map(p => ({
        sys_id: p.sys_id,
        description: p.short_description,
        conditions: p.conditions,
        actions: policyActions
          .filter(a => {
            const pId = typeof a.ui_policy === 'object' ? a.ui_policy.value : a.ui_policy;
            return pId === p.sys_id;
          })
          .map(a => ({
            variable: typeof a.catalog_variable === 'object'
              ? (a.catalog_variable.display_value || a.catalog_variable.value)
              : a.catalog_variable,
            mandatory: a.mandatory,
            visible: a.visible,
            read_only: a.read_only
          }))
      })),
      client_scripts: clientScripts.map(s => ({
        name: s.name || s.script_name,
        type: s.type,
        applies_to: s.applies_to
      }))
    };
  }

  /**
   * Search catalog items and record producers by keyword and/or category.
   * @param {object} query - { keyword, category, active, limit, include_producers }
   * @returns {Array} Matching catalog items/record producers
   */
  async searchCatalogItems(query = {}) {
    const {
      keyword,
      category,
      active = true,
      limit = 25,
      include_producers = true
    } = query;

    const conditions = [];
    if (active !== null && active !== undefined) {
      conditions.push(`active=${active}`);
    }
    if (category) {
      conditions.push(`category=${category}`);
    }
    if (keyword) {
      conditions.push(`nameLIKE${keyword}^ORshort_descriptionLIKE${keyword}`);
    }

    // Default to active=true when no conditions were specified
    const sysparmQuery = conditions.length > 0 ? conditions.join('^') : 'active=true';
    const fields = 'sys_id,name,short_description,category,active,sys_class_name';

    const fetchParams = {
      sysparm_query: sysparmQuery,
      sysparm_fields: fields,
      sysparm_limit: limit
    };

    const items = await this.getRecords('sc_cat_item', fetchParams);

    // Optionally also fetch record producers (separate table in some SN versions)
    let producers = [];
    if (include_producers) {
      try {
        producers = await this.getRecords('sc_cat_item_producer', {
          sysparm_query: sysparmQuery,
          sysparm_fields: fields,
          sysparm_limit: limit
        });
      } catch (e) {
        // sc_cat_item_producer may not exist or be accessible in all SN versions
        console.error(`⚠️  Could not query sc_cat_item_producer: ${e.message}`);
      }
    }

    const normalize = (record, defaultType) => ({
      sys_id: record.sys_id,
      name: record.name,
      short_description: record.short_description,
      category: record.category,
      active: record.active,
      item_type: record.sys_class_name === 'sc_cat_item_producer' ? 'record_producer' : defaultType
    });

    return [
      ...items.map(i => normalize(i, 'catalog_item')),
      ...producers.map(p => normalize(p, 'record_producer'))
    ];
  }

  /**
   * Submit a catalog item or record producer form via the Service Catalog REST API.
   * Works for both catalog items and record producers — the sn_sc endpoint handles both.
   * @param {string} sysId - Catalog item sys_id
   * @param {object} variables - Map of { variable_name: value }
   * @param {object} options - { requested_for, quantity }
   * @returns {object} Submission result including request/task numbers
   */
  async submitCatalogForm(sysId, variables = {}, options = {}) {
    const { requested_for, quantity = 1 } = options;

    const payload = {
      sysparm_quantity: quantity,
      variables
    };

    if (requested_for) {
      payload.requested_for = requested_for;
    }

    const response = await this.client.post(
      `/api/sn_sc/servicecatalog/items/${sysId}/order_now`,
      payload
    );

    return response.data.result;
  }

  // Add comments/work notes to any table
  async addComment(table, recordId, comment, isWorkNote = false) {
    const field = isWorkNote ? 'work_notes' : 'comments';
    const updateData = { [field]: comment };
    return this.updateRecord(table, recordId, updateData);
  }

  // Generic search across tables
  async searchRecords(table, searchTerm, fields = [], limit = 10) {
    const searchQuery = fields.length > 0
      ? fields.map(field => `${field}CONTAINS${searchTerm}`).join('^OR')
      : `short_descriptionCONTAINS${searchTerm}^ORdescriptionCONTAINS${searchTerm}`;

    return this.getRecords(table, {
      sysparm_query: searchQuery,
      sysparm_limit: limit
    });
  }

  // Configuration Items (CMDB)
  async getConfigurationItems(query = {}) {
    return this.getRecords('cmdb_ci', query);
  }

  async getConfigurationItem(sysId) {
    return this.getRecord('cmdb_ci', sysId);
  }

  async createConfigurationItem(data) {
    return this.createRecord('cmdb_ci', data);
  }

  async updateConfigurationItem(sysId, data) {
    return this.updateRecord('cmdb_ci', sysId, data);
  }

  // Business Rules
  async getBusinessRules(query = {}) {
    return this.getRecords('sys_script', query);
  }

  async createBusinessRule(data) {
    return this.createRecord('sys_script', data);
  }

  async updateBusinessRule(sysId, data) {
    return this.updateRecord('sys_script', sysId, data);
  }

  // Update Sets
  async getUpdateSets(query = {}) {
    return this.getRecords('sys_update_set', query);
  }

  async createUpdateSet(data) {
    return this.createRecord('sys_update_set', data);
  }

  async updateUpdateSet(sysId, data) {
    return this.updateRecord('sys_update_set', sysId, data);
  }

  // Workflows
  async getWorkflows(query = {}) {
    return this.getRecords('wf_workflow', query);
  }

  async createWorkflow(data) {
    return this.createRecord('wf_workflow', data);
  }

  // Attachments
  async getAttachments(tableId, recordId) {
    return this.getRecords('sys_attachment', {
      sysparm_query: `table_name=${tableId}^table_sys_id=${recordId}`
    });
  }

  async createAttachment(tableName, recordId, fileName, contentType, data) {
    const formData = new FormData();
    formData.append('table_name', tableName);
    formData.append('table_sys_id', recordId);
    formData.append('file_name', fileName);
    formData.append('content_type', contentType);
    formData.append('content', data);

    const response = await this.client.post('/api/now/attachment/file', formData, {
      headers: {
        'Content-Type': 'multipart/form-data'
      }
    });
    return response.data.result;
  }

  // SLA Management
  async getSLAs(query = {}) {
    return this.getRecords('contract_sla', query);
  }

  async getSLADefinitions(query = {}) {
    return this.getRecords('sla_definition', query);
  }

  // Task Assignment
  async assignTask(table, recordId, assignedTo, assignmentGroup = null) {
    const updateData = { assigned_to: assignedTo };
    if (assignmentGroup) {
      updateData.assignment_group = assignmentGroup;
    }
    return this.updateRecord(table, recordId, updateData);
  }

  // Reports
  async getReports(query = {}) {
    return this.getRecords('sys_report', query);
  }

  async createReport(data) {
    return this.createRecord('sys_report', data);
  }

  // Execute script via sys_trigger (scheduled job that runs immediately).
  //
  // When `options.wait` is true (the default), this blocks until the
  // script's completion marker shows up in syslog (or the timeout budget
  // is exhausted) and resolves to one of three distinguishable outcomes:
  //   - status: 'completed' — the script ran and did not throw; `output`
  //     carries whatever accompanied the success marker, and `logs`
  //     carries whatever the script itself logged via gs.info/gs.print.
  //   - status: 'failed'    — the script ran and threw; `success` is false,
  //     `error`/`stack` carry the failure, and `logs` still carries
  //     whatever the script logged before it threw. This is NOT a success.
  //   - status: 'timeout'   — no completion marker appeared within the
  //     budget; `success` is false. The trigger may still be pending, may
  //     have run without a marker, or may never have fired — inspect
  //     `trigger_sys_id` manually. There is no end marker to bound a
  //     window with, so `logs` is omitted rather than guessed.
  // Pass `{ wait: false }` to keep the original fire-and-forget behavior
  // (status: 'scheduled', no polling, no logs).
  async executeScriptViaTrigger(script, description = 'MCP Script Execution', autoDelete = true, options = {}) {
    const { wait = true, timeoutMs = 15000, intervalMs = 500 } = options;
    try {
      // Calculate next action time (1 second from now)
      const now = new Date();
      const nextAction = new Date(now.getTime() + 1000); // 1 second from now

      // Format: YYYY-MM-DD HH:MM:SS (UTC — the Table API interprets a raw
      // glide_date_time string as UTC, so this must not use local getters)
      const formatDateTime = (date) => date.toISOString().slice(0, 19).replace('T', ' ');

      // Unique per-execution marker so the completion line is greppable in
      // syslog and cannot be confused with output from an unrelated run.
      const marker = generateScriptExecutionMarker();
      const endMarker = `${marker}:END`;
      // Bound the syslog poll to this execution window — an unbounded
      // syslog query 500s on this platform (see issue #40).
      const pollSince = formatDateTime(new Date(now.getTime() - 2000));

      const baseTriggerFields = {
        name: `MCP_Script_${Date.now()}`,
        next_action: formatDateTime(nextAction),
        trigger_type: '0', // Run once
        state: '0', // Ready state
        description: description || 'Automated script execution via MCP'
      };

      let trigger;
      if (autoDelete) {
        // The self-delete block needs the trigger's own sys_id, which only
        // exists after creation — create first, then patch in the wrapped
        // script that references its own sys_id.
        trigger = await this.createRecord('sys_trigger', { ...baseTriggerFields, script });
        const wrappedScript = buildBackgroundScriptWrapper({ script, marker, autoDelete: true, triggerSysId: trigger.sys_id });
        await this.updateRecord('sys_trigger', trigger.sys_id, { script: wrappedScript });
      } else {
        const wrappedScript = buildBackgroundScriptWrapper({ script, marker, autoDelete: false });
        trigger = await this.createRecord('sys_trigger', { ...baseTriggerFields, script: wrappedScript });
      }

      const schedulingInfo = {
        trigger_sys_id: trigger.sys_id,
        trigger_name: trigger.name,
        next_action: formatDateTime(nextAction),
        auto_delete: autoDelete
      };

      if (!wait) {
        return {
          success: true,
          status: 'scheduled',
          ...schedulingInfo,
          message: `Script scheduled to run at ${formatDateTime(nextAction)}. ${autoDelete ? 'Trigger will auto-delete after execution.' : 'Trigger will remain after execution.'}`
        };
      }

      const outcome = await this._pollBackgroundScriptOutcome(endMarker, pollSince, timeoutMs, intervalMs);

      if (outcome.state === 'completed' || outcome.state === 'failed') {
        const { logs, truncated, logsWindowUnverified } = await this._collectBackgroundScriptLogs({
          endMarker,
          sinceDateTime: schedulingInfo.next_action,
          untilDateTime: outcome.endTime
        });

        if (outcome.state === 'completed') {
          return {
            success: true,
            status: 'completed',
            ...schedulingInfo,
            output: outcome.output,
            logs,
            ...(truncated ? { logsTruncated: true } : {}),
            ...(logsWindowUnverified ? { logsWindowUnverified: true } : {}),
            message: 'Script executed successfully.'
          };
        }

        return {
          success: false,
          status: 'failed',
          ...schedulingInfo,
          error: outcome.error,
          stack: outcome.stack,
          logs,
          ...(truncated ? { logsTruncated: true } : {}),
          ...(logsWindowUnverified ? { logsWindowUnverified: true } : {}),
          message: `Script execution failed: ${outcome.error}`
        };
      }

      return {
        success: false,
        status: 'timeout',
        ...schedulingInfo,
        message: `Timed out after ${timeoutMs}ms waiting for the script's completion marker in syslog. Trigger ${trigger.sys_id} may still be pending, may have run without producing a marker, or may have failed to fire — inspect it manually.`
      };
    } catch (error) {
      throw new Error(`Failed to create script trigger: ${error.message}`);
    }
  }

  // Polls syslog for the completion marker a background script wrapper
  // (see buildBackgroundScriptWrapper) writes on success or failure. The
  // sys_created_on lower bound is required: an unbounded syslog query 500s
  // on this platform (see issue #40). Returns the found record's
  // `sys_created_on` as `endTime` so the caller can bound a log-collection
  // window without a second guess at the script's finish time.
  async _pollBackgroundScriptOutcome(endMarker, sinceDateTime, timeoutMs, intervalMs) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const records = await this.getRecords('syslog', {
        sysparm_query: `messageLIKE${endMarker}^sys_created_on>=${sinceDateTime}^ORDERBYDESCsys_created_on`,
        sysparm_fields: 'message,sys_created_on',
        sysparm_limit: 1
      });

      if (Array.isArray(records) && records.length > 0) {
        return { ...parseBackgroundScriptMarker(records[0], endMarker), endTime: records[0].sys_created_on };
      }

      if (Date.now() >= deadline) {
        return { state: 'timeout' };
      }

      await new Promise(resolve => setTimeout(resolve, intervalMs));
    }
  }

  // Collects whatever the script itself logged (gs.info/gs.print) between
  // its start and end markers, for a `completed`/`failed` outcome only — a
  // `timeout` has no end marker and therefore no window to bound a query
  // with. The user's own log lines carry no marker text, so a broad
  // `sys_created_on` range query (both a lower AND an upper bound — an
  // unbounded syslog query 500s on this platform) is unavoidable to find
  // them.
  //
  // Two platform limitations were found by testing this against a live
  // instance, and both shape the implementation below:
  //
  // 1. `sys_created_on` is second-resolution, and ties are NOT guaranteed
  //    to come back in real execution order — a positional "slice between
  //    this run's own start/end line" approach (tried first) silently
  //    scrambled when two runs landed in the same second, leaking one
  //    run's own marker text into another run's `logs`. So marker lines
  //    are excluded by SHAPE (any `MCP_<ts>_<hex>:START`/`:END` line),
  //    not by exact match to this run's own marker — this guarantees no
  //    execution's bookkeeping literal ever leaks into `logs`, regardless
  //    of ordering.
  // 2. The lower bound is the trigger's own `next_action` (the earliest
  //    moment this run's script could possibly execute), not a generic
  //    few-seconds-back buffer — a generic buffer let an EARLIER, already
  //    finished MCP call's own log lines bleed into this run's window.
  //
  // What is NOT solved, and cannot be from the client side: two genuinely
  // unrelated background scripts (this run's, and some other scheduled
  // job on a busy/shared instance) both logging with `source: '*** Script'`
  // inside the exact same second are indistinguishable by timestamp alone,
  // and neither carries a marker to filter on. That residual noise is a
  // real, disclosed limitation of syslog time-window correlation on this
  // platform, not a bug — solving it would require a server-side
  // correlation id the Table API does not expose.
  //
  // Integrity check: `sinceDateTime` (this run's own `next_action`) is
  // computed from the MCP host's clock, with no slack for host/instance
  // clock skew — unlike the outcome poll's lower bound, which has a
  // built-in buffer. If the host clock runs ahead of the instance, this
  // bound can land AFTER the real ServiceNow-assigned timestamps of this
  // run's own log rows, and the range query below would then miss them
  // entirely — including this run's own end marker, which
  // `_pollBackgroundScriptOutcome` JUST found moments ago via a separate,
  // looser query. Its absence here is therefore a hard signal that the
  // window is wrong, not that the script printed nothing: an empty
  // `logs` array would otherwise be silently indistinguishable from a
  // genuinely silent script — precisely the ambiguity issue #40 exists
  // to resolve. `endMarker` is passed in solely for this check, not for
  // content filtering (that stays shape-based per point 1 above).
  async _collectBackgroundScriptLogs({ endMarker, sinceDateTime, untilDateTime, maxLines = 100 }) {
    const records = await this.getRecords('syslog', {
      sysparm_query: `source=*** Script^sys_created_on>=${sinceDateTime}^sys_created_on<=${untilDateTime}^ORDERBYsys_created_on`,
      sysparm_fields: 'message,sys_created_on',
      sysparm_limit: maxLines + 10
    });

    const messages = (Array.isArray(records) ? records : []).map(record => record?.message || '');
    const windowVerified = messages.some(message => message.startsWith(endMarker));

    const lines = messages.filter(message => !MCP_SCRIPT_MARKER_PATTERN.test(message));

    const truncated = lines.length > maxLines;
    return {
      logs: truncated ? lines.slice(0, maxLines) : lines,
      truncated,
      ...(windowVerified ? {} : { logsWindowUnverified: true })
    };
  }

  // Batch operations
  async batchCreate(operations, transaction = true, reportProgress = true) {
    const results = {
      success: true,
      created_count: 0,
      sys_ids: {},
      errors: [],
      execution_time_ms: 0
    };

    const startTime = Date.now();
    const total = operations.length;

    // Determine progress reporting frequency
    const shouldReport = (index) => {
      if (!reportProgress) return false;
      if (total <= 10) return true; // Report every item for small batches
      if (total <= 50) return (index + 1) % 5 === 0 || index === total - 1; // Every 5 items
      return (index + 1) % Math.ceil(total / 10) === 0 || index === total - 1; // Every 10%
    };

    try {
      for (let i = 0; i < operations.length; i++) {
        const op = operations[i];
        try {
          // Replace variable references from previous operations
          let processedData = JSON.stringify(op.data);
          Object.keys(results.sys_ids).forEach(key => {
            processedData = processedData.replace(`\${${key}}`, results.sys_ids[key]);
          });
          const data = JSON.parse(processedData);

          const result = await this.createRecord(op.table, data);

          // Save sys_id with the save_as key or operation index
          const key = op.save_as || `operation_${i}`;
          results.sys_ids[key] = result.sys_id;
          results.created_count++;

          // Report progress
          if (shouldReport(i)) {
            const percentage = Math.round(((i + 1) / total) * 100);
            this.notifyProgress(`Creating record ${i + 1}/${total} (${percentage}%): ${op.table}`, i + 1, total);
          }
        } catch (error) {
          results.errors.push({
            operation_index: i,
            table: op.table,
            error: error.message
          });

          if (reportProgress) {
            this.notifyProgress(`Failed ${i + 1}/${total}: ${op.table} - ${error.message}`, i + 1, total);
          }

          if (transaction) {
            results.success = false;
            throw new Error(`Batch create failed at operation ${i}: ${error.message}`);
          }
        }
      }

      // Final summary
      if (reportProgress) {
        const failedCount = results.errors.length;
        if (failedCount > 0) {
          this.notifyProgress(`Complete: ${results.created_count}/${total} records created (${failedCount} failed)`);
        } else {
          this.notifyProgress(`Complete: All ${total} records created successfully`);
        }
      }
    } finally {
      results.execution_time_ms = Date.now() - startTime;
    }

    return results;
  }

  async batchUpdate(updates, stopOnError = false, reportProgress = true) {
    const results = {
      success: true,
      updated_count: 0,
      errors: [],
      execution_time_ms: 0
    };

    const startTime = Date.now();
    const total = updates.length;

    // Determine progress reporting frequency
    const shouldReport = (index) => {
      if (!reportProgress) return false;
      if (total <= 10) return true; // Report every item for small batches
      if (total <= 50) return (index + 1) % 5 === 0 || index === total - 1; // Every 5 items
      return (index + 1) % Math.ceil(total / 10) === 0 || index === total - 1; // Every 10%
    };

    for (let i = 0; i < updates.length; i++) {
      const update = updates[i];
      try {
        await this.updateRecord(update.table, update.sys_id, update.data);
        results.updated_count++;

        // Report progress
        if (shouldReport(i)) {
          const percentage = Math.round(((i + 1) / total) * 100);
          this.notifyProgress(`Updating record ${i + 1}/${total} (${percentage}%): ${update.table}`, i + 1, total);
        }
      } catch (error) {
        results.errors.push({
          update_index: i,
          table: update.table,
          sys_id: update.sys_id,
          error: error.message
        });

        if (reportProgress) {
          this.notifyProgress(`Failed ${i + 1}/${total}: ${update.table} - ${error.message}`, i + 1, total);
        }

        if (stopOnError) {
          results.success = false;
          break;
        }
      }
    }

    // Final summary
    if (reportProgress) {
      const failedCount = results.errors.length;
      if (failedCount > 0) {
        this.notifyProgress(`Complete: ${results.updated_count}/${total} records updated (${failedCount} failed)`);
      } else {
        this.notifyProgress(`Complete: All ${total} records updated successfully`);
      }
    }

    results.execution_time_ms = Date.now() - startTime;
    return results;
  }

  // Workflow creation methods
  async createWorkflow(workflowData) {
    // Create base workflow
    const workflow = await this.createRecord('wf_workflow', {
      name: workflowData.name,
      description: workflowData.description || '',
      template: workflowData.template || false,
      access: workflowData.access || 'public'
    });

    return {
      workflow_sys_id: workflow.sys_id,
      name: workflowData.name
    };
  }

  async createWorkflowVersion(versionData) {
    // Create workflow version
    const version = await this.createRecord('wf_workflow_version', {
      name: versionData.name,
      workflow: versionData.workflow_sys_id,
      table: versionData.table,
      description: versionData.description || '',
      active: versionData.active !== undefined ? versionData.active : true,
      published: versionData.published || false,
      condition: versionData.condition || '',
      order: versionData.order || 100,
      run_multiple: versionData.run_multiple || false,
      after_business_rules: versionData.after_business_rules !== undefined ? versionData.after_business_rules : true,
      expected_time: versionData.expected_time || '',
      condition_type: versionData.condition_type || ''
    });

    return {
      version_sys_id: version.sys_id,
      name: versionData.name
    };
  }

  async createActivity(activityData) {
    // Create activity
    const activity = await this.createRecord('wf_activity', {
      name: activityData.name,
      workflow_version: activityData.workflow_version_sys_id,
      activity_definition: activityData.activity_definition_sys_id || '',
      x: activityData.x || 100,
      y: activityData.y || 100,
      width: activityData.width || 150,
      height: activityData.height || 80,
      input: activityData.script || activityData.input || '',
      vars: activityData.vars || '',
      stage: activityData.stage_sys_id || '',
      parent: activityData.parent_sys_id || '',
      timeout: activityData.timeout || '0 00:00:00'
    });

    return {
      activity_sys_id: activity.sys_id,
      name: activityData.name
    };
  }

  async createCondition(conditionData) {
    // Create condition
    const condition = await this.createRecord('wf_condition', {
      activity: conditionData.activity_sys_id,
      name: conditionData.name,
      short_description: conditionData.description || '',
      condition: conditionData.condition || '',
      order: conditionData.order || 1,
      else_flag: conditionData.else_flag || false,
      event: conditionData.event || false,
      event_name: conditionData.event_name || '',
      condition_type: conditionData.condition_type || 'standard'
    });

    return {
      condition_sys_id: condition.sys_id,
      name: conditionData.name
    };
  }

  async createTransition(transitionData) {
    // Create transition
    const transition = await this.createRecord('wf_transition', {
      from: transitionData.from_activity_sys_id,
      to: transitionData.to_activity_sys_id,
      condition: transitionData.condition_sys_id || '',
      order: transitionData.order || 1
    });

    return {
      transition_sys_id: transition.sys_id
    };
  }

  async publishWorkflow(versionSysId, startActivitySysId) {
    // Update workflow version to set start activity and publish
    const updated = await this.updateRecord('wf_workflow_version', versionSysId, {
      start: startActivitySysId,
      published: true
    });

    return {
      version_sys_id: versionSysId,
      published: true,
      start_activity: startActivitySysId
    };
  }

  async createCompleteWorkflow(workflowSpec, reportProgress = true) {
    // Create complete workflow with activities and transitions in one call
    const results = {
      workflow_sys_id: '',
      version_sys_id: '',
      activity_sys_ids: {},
      transition_sys_ids: [],
      published: false
    };

    try {
      // 1. Create base workflow
      if (reportProgress) this.notifyProgress('Creating workflow base');
      const workflow = await this.createWorkflow({
        name: workflowSpec.name,
        description: workflowSpec.description,
        template: workflowSpec.template,
        access: workflowSpec.access
      });
      results.workflow_sys_id = workflow.workflow_sys_id;

      // 2. Create workflow version
      if (reportProgress) this.notifyProgress('Creating workflow version');
      const version = await this.createWorkflowVersion({
        name: workflowSpec.name,
        workflow_sys_id: workflow.workflow_sys_id,
        table: workflowSpec.table,
        description: workflowSpec.description,
        active: workflowSpec.active,
        published: false,  // Don't publish yet
        condition: workflowSpec.condition,
        order: workflowSpec.order,
        run_multiple: workflowSpec.run_multiple,
        after_business_rules: workflowSpec.after_business_rules
      });
      results.version_sys_id = version.version_sys_id;

      // 3. Create activities
      const activities = workflowSpec.activities || [];
      const totalActivities = activities.length;
      for (let i = 0; i < activities.length; i++) {
        const actSpec = activities[i];

        if (reportProgress) {
          this.notifyProgress(`Creating activity ${i + 1}/${totalActivities}: ${actSpec.name}`, i + 1, totalActivities);
        }

        const activity = await this.createActivity({
          name: actSpec.name,
          workflow_version_sys_id: version.version_sys_id,
          activity_definition_sys_id: actSpec.activity_type,
          x: actSpec.x !== undefined ? actSpec.x : (100 + i * 150),
          y: actSpec.y !== undefined ? actSpec.y : 100,
          width: actSpec.width,
          height: actSpec.height,
          script: actSpec.script,
          vars: actSpec.vars,
          stage_sys_id: actSpec.stage,
          parent_sys_id: actSpec.parent,
          timeout: actSpec.timeout
        });

        const key = actSpec.id || `activity_${i}`;
        results.activity_sys_ids[key] = activity.activity_sys_id;
      }

      // 4. Create transitions
      const transitions = workflowSpec.transitions || [];
      const totalTransitions = transitions.length;
      for (let i = 0; i < transitions.length; i++) {
        const transSpec = transitions[i];

        if (reportProgress) {
          this.notifyProgress(`Creating transition ${i + 1}/${totalTransitions}`, i + 1, totalTransitions);
        }

        // Resolve activity references
        const fromId = typeof transSpec.from === 'number'
          ? results.activity_sys_ids[`activity_${transSpec.from}`]
          : results.activity_sys_ids[transSpec.from] || transSpec.from;

        const toId = typeof transSpec.to === 'number'
          ? results.activity_sys_ids[`activity_${transSpec.to}`]
          : results.activity_sys_ids[transSpec.to] || transSpec.to;

        // Create condition if specified
        let conditionId = transSpec.condition_sys_id;
        if (transSpec.condition && !conditionId) {
          const condition = await this.createCondition({
            activity_sys_id: fromId,
            name: transSpec.condition_name || 'Condition',
            description: transSpec.condition_description,
            condition: transSpec.condition,
            order: transSpec.order,
            else_flag: transSpec.else_flag
          });
          conditionId = condition.condition_sys_id;
        }

        // Create transition
        const transition = await this.createTransition({
          from_activity_sys_id: fromId,
          to_activity_sys_id: toId,
          condition_sys_id: conditionId,
          order: transSpec.order
        });

        results.transition_sys_ids.push(transition.transition_sys_id);
      }

      // 5. Publish if requested
      if (workflowSpec.publish && activities.length > 0) {
        if (reportProgress) this.notifyProgress('Publishing workflow');

        const startActivityId = workflowSpec.start_activity
          ? (results.activity_sys_ids[workflowSpec.start_activity] || workflowSpec.start_activity)
          : results.activity_sys_ids['activity_0'] || results.activity_sys_ids[Object.keys(results.activity_sys_ids)[0]];

        await this.publishWorkflow(version.version_sys_id, startActivityId);
        results.published = true;
        results.start_activity = startActivityId;
      }

      if (reportProgress) {
        this.notifyProgress(`Complete: Workflow created with ${totalActivities} activities and ${totalTransitions} transitions`);
      }

      return results;
    } catch (error) {
      throw new Error(`Failed to create workflow: ${error.message}`);
    }
  }

  // Move records to update set
  async moveRecordsToUpdateSet(updateSetId, options = {}) {
    const {
      record_sys_ids = [],
      time_range = null,
      source_update_set = null,
      table = 'sys_update_xml',
      reportProgress = true
    } = options;

    try {
      const results = {
        moved: 0,
        failed: 0,
        records: [],
        errors: []
      };

      let recordsToMove = [];

      // Get records by sys_ids
      if (record_sys_ids.length > 0) {
        if (reportProgress) this.notifyProgress(`Fetching ${record_sys_ids.length} records to move`);
        const sysIdsQuery = record_sys_ids.map(id => `sys_id=${id}`).join('^OR');
        recordsToMove = await this.getRecords(table, {
          sysparm_query: sysIdsQuery,
          sysparm_limit: 1000
        });
      }
      // Get records by time range
      else if (time_range) {
        if (reportProgress) this.notifyProgress('Fetching records by time range');
        let query = `sys_created_on>=${time_range.start}^sys_created_on<=${time_range.end}`;
        if (source_update_set) {
          query += `^update_set.name=${source_update_set}`;
        }
        recordsToMove = await this.getRecords(table, {
          sysparm_query: query,
          sysparm_limit: 1000
        });
      }

      const total = recordsToMove.length;
      if (total === 0) {
        if (reportProgress) this.notifyProgress('No records found to move');
        return results;
      }

      if (reportProgress) this.notifyProgress(`Moving ${total} records to update set`);

      // Determine progress reporting frequency
      const shouldReport = (index) => {
        if (!reportProgress) return false;
        if (total <= 10) return true;
        if (total <= 50) return (index + 1) % 5 === 0 || index === total - 1;
        return (index + 1) % Math.ceil(total / 10) === 0 || index === total - 1;
      };

      // Move each record
      for (let i = 0; i < recordsToMove.length; i++) {
        const record = recordsToMove[i];
        try {
          await this.updateRecord(table, record.sys_id, {
            update_set: updateSetId
          });
          results.moved++;
          results.records.push({
            sys_id: record.sys_id,
            name: record.name,
            type: record.type,
            status: 'moved'
          });

          if (shouldReport(i)) {
            const percentage = Math.round(((i + 1) / total) * 100);
            this.notifyProgress(`Moving record ${i + 1}/${total} (${percentage}%): ${record.type || 'unknown'}`, i + 1, total);
          }
        } catch (error) {
          results.failed++;
          results.errors.push({
            sys_id: record.sys_id,
            error: error.message
          });

          if (reportProgress) {
            this.notifyProgress(`Failed ${i + 1}/${total}: ${record.sys_id} - ${error.message}`, i + 1, total);
          }
        }
      }

      if (reportProgress) {
        if (results.failed > 0) {
          this.notifyProgress(`Complete: ${results.moved}/${total} records moved (${results.failed} failed)`);
        } else {
          this.notifyProgress(`Complete: All ${total} records moved successfully`);
        }
      }

      return results;
    } catch (error) {
      throw new Error(`Failed to move records to update set: ${error.message}`);
    }
  }

  // Clone update set
  async cloneUpdateSet(sourceUpdateSetId, newName, reportProgress = true) {
    try {
      // Get source update set
      if (reportProgress) this.notifyProgress('Fetching source update set');
      const sourceSet = await this.getRecord('sys_update_set', sourceUpdateSetId);

      // Create new update set
      if (reportProgress) this.notifyProgress(`Creating new update set: ${newName}`);
      const newSet = await this.createRecord('sys_update_set', {
        name: newName,
        description: `Clone of: ${sourceSet.name}\n\n${sourceSet.description || ''}`,
        application: sourceSet.application,
        state: 'in progress'
      });

      // Get all update XML records from source
      if (reportProgress) this.notifyProgress('Fetching update records from source');
      const updateRecords = await this.getRecords('sys_update_xml', {
        sysparm_query: `update_set=${sourceUpdateSetId}`,
        sysparm_limit: 5000
      });

      const total = updateRecords.length;
      if (reportProgress) this.notifyProgress(`Cloning ${total} update records`);

      // Determine progress reporting frequency
      const shouldReport = (index) => {
        if (!reportProgress) return false;
        if (total <= 10) return true;
        if (total <= 50) return (index + 1) % 5 === 0 || index === total - 1;
        return (index + 1) % Math.ceil(total / 10) === 0 || index === total - 1;
      };

      // Clone each update record (create new records pointing to new set)
      const clonedRecords = [];
      let failedCount = 0;
      for (let i = 0; i < updateRecords.length; i++) {
        const record = updateRecords[i];
        try {
          const cloned = await this.createRecord('sys_update_xml', {
            update_set: newSet.sys_id,
            name: record.name,
            type: record.type,
            target_name: record.target_name,
            payload: record.payload,
            category: record.category
          });
          clonedRecords.push(cloned);

          if (shouldReport(i)) {
            const percentage = Math.round(((i + 1) / total) * 100);
            this.notifyProgress(`Cloning record ${i + 1}/${total} (${percentage}%): ${record.type || 'unknown'}`, i + 1, total);
          }
        } catch (error) {
          failedCount++;
          console.error(`Failed to clone record ${record.sys_id}: ${error.message}`);
          if (reportProgress && failedCount <= 5) { // Only report first 5 failures to avoid spam
            this.notifyProgress(`Failed to clone record ${i + 1}/${total}: ${error.message}`, i + 1, total);
          }
        }
      }

      if (reportProgress) {
        if (failedCount > 0) {
          this.notifyProgress(`Complete: ${clonedRecords.length}/${total} records cloned (${failedCount} failed)`);
        } else {
          this.notifyProgress(`Complete: All ${total} records cloned successfully`);
        }
      }

      return {
        new_update_set_id: newSet.sys_id,
        new_update_set_name: newSet.name,
        source_update_set_id: sourceUpdateSetId,
        source_update_set_name: sourceSet.name,
        records_cloned: clonedRecords.length,
        total_source_records: updateRecords.length
      };
    } catch (error) {
      throw new Error(`Failed to clone update set: ${error.message}`);
    }
  }

  // Enhanced schema discovery
  async discoverTableSchema(tableName, options = {}) {
    const {
      include_type_codes = false,
      include_choice_tables = false,
      include_relationships = false,
      include_ui_policies = false,
      include_business_rules = false,
      include_field_constraints = false
    } = options;

    const schema = {
      table: tableName,
      label: null,
      fields: []
    };

    try {
      // Get table metadata
      const tables = await this.getRecords('sys_db_object', {
        sysparm_query: `name=${tableName}`,
        sysparm_limit: 1
      });

      if (tables.length > 0) {
        schema.label = tables[0].label;
        schema.super_class = tables[0].super_class?.value;
      }

      // Get field definitions
      const fields = await this.getRecords('sys_dictionary', {
        sysparm_query: `name=${tableName}`,
        sysparm_limit: 1000
      });

      for (const field of fields) {
        const fieldInfo = {
          name: field.element,
          label: field.column_label,
          internal_type: field.internal_type?.value || field.internal_type,
          max_length: field.max_length,
          mandatory: field.mandatory === 'true' || field.mandatory === true,
          read_only: field.read_only === 'true' || field.read_only === true
        };

        // Add type codes for integer fields (like variable types)
        if (include_type_codes && fieldInfo.internal_type === 'integer' && field.element === 'type') {
          const choices = await this.getRecords('sys_choice', {
            sysparm_query: `name=${tableName}^element=type`,
            sysparm_limit: 100
          });

          if (choices.length > 0) {
            fieldInfo.type_codes = {};
            choices.forEach(choice => {
              fieldInfo.type_codes[choice.value] = choice.label;
            });
          }
        }

        // Add reference information
        if (fieldInfo.internal_type === 'reference' && field.reference) {
          fieldInfo.reference_table = field.reference.value || field.reference;
        }

        if (include_field_constraints && field.default_value) {
          fieldInfo.default_value = field.default_value;
        }

        schema.fields.push(fieldInfo);
      }

      // Choice tables
      if (include_choice_tables) {
        schema.choice_tables = {
          sys_choice: 'For table field choices',
          question_choice: 'For catalog variable choices'
        };
      }

      // Relationships
      if (include_relationships) {
        schema.relationships = {};
        const refFields = schema.fields.filter(f => f.internal_type === 'reference');
        for (const field of refFields) {
          if (field.reference_table) {
            schema.relationships[field.name] = {
              type: 'reference',
              table: field.reference_table,
              description: field.label
            };
          }
        }
      }

      // UI Policies
      if (include_ui_policies) {
        const policies = await this.getRecords('sys_ui_policy', {
          sysparm_query: `table=${tableName}^active=true`,
          sysparm_fields: 'sys_id,short_description',
          sysparm_limit: 100
        });
        schema.ui_policies = policies.map(p => ({
          sys_id: p.sys_id,
          description: p.short_description
        }));
      }

      // Business Rules
      if (include_business_rules) {
        const rules = await this.getRecords('sys_script', {
          sysparm_query: `collection=${tableName}^active=true`,
          sysparm_fields: 'sys_id,name,when',
          sysparm_limit: 100
        });
        schema.business_rules = rules.map(r => ({
          sys_id: r.sys_id,
          name: r.name,
          when: r.when
        }));
      }

    } catch (error) {
      throw new Error(`Failed to discover schema for ${tableName}: ${error.message}`);
    }

    return schema;
  }

  // Field explanation
  async explainField(tableName, fieldName, includeExamples = true) {
    try {
      const fields = await this.getRecords('sys_dictionary', {
        sysparm_query: `name=${tableName}^element=${fieldName}`,
        sysparm_limit: 1
      });

      if (fields.length === 0) {
        throw new Error(`Field ${fieldName} not found in table ${tableName}`);
      }

      const field = fields[0];
      const explanation = {
        field: fieldName,
        table: tableName,
        label: field.column_label,
        type: field.internal_type?.value || field.internal_type,
        max_length: field.max_length,
        mandatory: field.mandatory === 'true' || field.mandatory === true,
        read_only: field.read_only === 'true' || field.read_only === true,
        comments: field.comments,
        help: field.help
      };

      // Get reference info
      if (field.reference) {
        explanation.reference_table = field.reference.value || field.reference;
      }

      // Get choices for choice fields
      if (field.internal_type === 'choice' || field.internal_type === 'integer') {
        const choices = await this.getRecords('sys_choice', {
          sysparm_query: `name=${tableName}^element=${fieldName}`,
          sysparm_limit: 100
        });

        if (choices.length > 0) {
          explanation.choices = choices.map(c => ({
            value: c.value,
            label: c.label
          }));
        }
      }

      // Known issues for specific fields
      if (tableName === 'catalog_ui_policy_action' && (fieldName === 'ui_policy' || fieldName === 'catalog_variable')) {
        explanation.known_issues = [
          'Cannot be set via REST API - use background script with setValue()',
          fieldName === 'catalog_variable' ? 'Must include IO: prefix or linkage will fail' : null
        ].filter(Boolean);

        if (fieldName === 'catalog_variable') {
          explanation.special_format = 'IO:<variable_sys_id>';
          if (includeExamples) {
            explanation.example = 'IO:94ababd1c35432101fcbbd43e40131bf';
          }
        }
      }

      return explanation;
    } catch (error) {
      throw new Error(`Failed to explain field: ${error.message}`);
    }
  }

  // Validate catalog configuration
  async validateCatalogConfiguration(catalogItemSysId, checks = {}) {
    const results = {
      valid: true,
      issues: [],
      warnings: 0,
      errors: 0
    };

    try {
      // Validate variables
      if (checks.variables) {
        const variables = await this.getRecords('item_option_new', {
          sysparm_query: `cat_item=${catalogItemSysId}`,
          sysparm_limit: 1000
        });

        for (const variable of variables) {
          // Check if linked
          if (checks.variables.check_linked && !variable.cat_item) {
            results.issues.push({
              severity: 'error',
              component: 'variable',
              sys_id: variable.sys_id,
              issue: `Variable ${variable.name} is not linked to catalog item`,
              fix: 'Update cat_item field'
            });
            results.errors++;
            results.valid = false;
          }

          // Check tooltip length
          if (variable.tooltip && variable.tooltip.length > 40) {
            results.issues.push({
              severity: 'warning',
              component: 'variable',
              sys_id: variable.sys_id,
              issue: `Tooltip exceeds 40 characters and will be truncated (${variable.tooltip.length} chars)`,
              fix: 'Move detailed help to help_text field'
            });
            results.warnings++;
          }

          // Check for choices
          if (checks.variables.check_choices && (variable.type === '1' || variable.type === '5')) {
            const choices = await this.getRecords('question_choice', {
              sysparm_query: `question=${variable.sys_id}`,
              sysparm_limit: 1
            });

            if (choices.length === 0) {
              results.issues.push({
                severity: 'error',
                component: 'variable',
                sys_id: variable.sys_id,
                issue: `Variable ${variable.name} is type ${variable.type === '1' ? 'Choice' : 'Select Box'} but has no choices defined`,
                fix: 'Add choices via question_choice table'
              });
              results.errors++;
              results.valid = false;
            }
          }
        }
      }

      // Validate UI policies
      if (checks.ui_policies) {
        const policies = await this.getRecords('catalog_ui_policy', {
          sysparm_query: `catalog_item=${catalogItemSysId}`,
          sysparm_limit: 1000
        });

        for (const policy of policies) {
          if (checks.ui_policies.check_actions_linked) {
            const actions = await this.getRecords('catalog_ui_policy_action', {
              sysparm_query: `ui_policy=${policy.sys_id}`,
              sysparm_limit: 1000
            });

            for (const action of actions) {
              if (!action.catalog_variable || action.catalog_variable === '') {
                results.issues.push({
                  severity: 'error',
                  component: 'ui_policy_action',
                  sys_id: action.sys_id,
                  issue: 'catalog_variable field is empty - action not linked to policy',
                  fix: 'Run background script to set catalog_variable value'
                });
                results.errors++;
                results.valid = false;
              }
            }
          }
        }
      }

    } catch (error) {
      results.issues.push({
        severity: 'error',
        component: 'validation',
        issue: `Validation failed: ${error.message}`
      });
      results.errors++;
      results.valid = false;
    }

    return results;
  }

  // Inspect update set
  async inspectUpdateSet(updateSetSysId, options = {}) {
    const {
      show_components = true,
      show_dependencies = false
    } = options;

    try {
      const updateSet = await this.getRecord('sys_update_set', updateSetSysId);

      const result = {
        update_set: {
          sys_id: updateSet.sys_id,
          name: updateSet.name,
          state: updateSet.state,
          description: updateSet.description
        },
        total_records: 0,
        components: []
      };

      if (show_components) {
        const updates = await this.getRecords('sys_update_xml', {
          sysparm_query: `update_set=${updateSetSysId}`,
          sysparm_fields: 'type,name,target_name',
          sysparm_limit: 10000
        });

        result.total_records = updates.length;

        // Group by type
        const typeGroups = {};
        updates.forEach(update => {
          const type = update.type || 'unknown';
          if (!typeGroups[type]) {
            typeGroups[type] = [];
          }
          typeGroups[type].push(update);
        });

        result.components = Object.keys(typeGroups).map(type => ({
          type,
          count: typeGroups[type].length,
          items: typeGroups[type].slice(0, 10).map(u => u.name || u.target_name)
        }));
      }

      result.ready_to_deploy = result.update_set.state === 'complete';

      return result;
    } catch (error) {
      throw new Error(`Failed to inspect update set: ${error.message}`);
    }
  }
}
