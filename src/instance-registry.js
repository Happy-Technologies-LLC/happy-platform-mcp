import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { credentialRefFor, parseCredentialRef } from './instance-credential-store.js';
import { resolveConfigPaths } from './config-path.js';
import { assertApprovedOAuthEndpoint } from './oauth-endpoint-policy.js';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const DEFAULT_LEGACY_PATH = path.resolve(__dirname, '../config/servicenow-instances.json');

const ALLOWED_FIELDS = new Set([
  'name', 'url', 'authType', 'grantType', 'username', 'clientId',
  'credentialRef', 'scope', 'authorizeUrl', 'tokenUrl', 'redirectPort',
  'callbackPath', 'default', 'description'
]);
const LEGACY_INSTANCE_SECRET_FIELDS = new Set(['password', 'clientSecret']);
const SECRET_KEY_PATTERN = /(?:privatekey|clientsecret|secret|password|accesstoken|refreshtoken|apikey|token)/i;
const CANONICAL_CREDENTIAL_FIELDS = new Set(['password', 'clientSecret']);
const GRANT_TYPES = new Set(['client_credentials', 'password', 'authorization_code']);
const AUTH_TYPES = new Set(['basic', 'oauth']);
const NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/;

export function isSecretShapedKey(key) {
  if (typeof key !== 'string') return false;
  const normalized = key.replace(/[-_]/g, '').toLowerCase();
  if (normalized === 'tokenurl' || normalized === 'credentialref') return false;
  return SECRET_KEY_PATTERN.test(normalized);
}

function isCanonicalCredentialReference(value) {
  try {
    parseCredentialRef(value);
    return true;
  } catch {
    return false;
  }
}

function redactCredentialReference(value) {
  if (isCanonicalCredentialReference(value)) return value;
  if (!value || typeof value !== 'object' || Array.isArray(value)) return undefined;
  const result = {};
  for (const key of CANONICAL_CREDENTIAL_FIELDS) {
    if (!Object.prototype.hasOwnProperty.call(value, key)) continue;
    if (!isCanonicalCredentialReference(value[key])) return undefined;
    result[key] = value[key];
  }
  return Object.keys(result).length ? result : undefined;
}

const mutationQueues = new Map();

function enqueuePathMutation(writePath, operation) {
  const previous = mutationQueues.get(writePath) || Promise.resolve();
  const mutation = previous.catch(() => undefined).then(operation);
  mutationQueues.set(writePath, mutation);
  mutation.then(
    () => {
      if (mutationQueues.get(writePath) === mutation) mutationQueues.delete(writePath);
    },
    () => {
      if (mutationQueues.get(writePath) === mutation) mutationQueues.delete(writePath);
    }
  );
  return mutation;
}

function clone(value) {
  return value === undefined ? value : JSON.parse(JSON.stringify(value));
}

function isValidCredentialRef(instance) {
  if (!Object.prototype.hasOwnProperty.call(instance || {}, 'credentialRef')) return true;
  try {
    const name = instance.name;
    const authType = instance.authType || 'basic';
    if (authType === 'basic') {
      canonicalCredentialRef(instance.credentialRef, name, 'password');
      return true;
    }
    const grantType = instance.grantType === undefined
      ? (instance.username ? 'password' : 'client_credentials')
      : instance.grantType;
    if (grantType === 'authorization_code') return false;
    if (grantType === 'client_credentials') {
      canonicalCredentialRef(instance.credentialRef, name, 'client-secret');
      return true;
    }
    passwordGrantRefs(instance.credentialRef, name);
    return true;
  } catch {
    return false;
  }
}

function redactInstanceSecrets(instance) {
  if (!instance || typeof instance !== 'object') return instance;
  const redacted = redactSecrets(instance);
  if (!isValidCredentialRef(instance)) delete redacted.credentialRef;
  return redacted;
}

function redactSecrets(value) {
  if (Array.isArray(value)) return value.map(nested => redactSecrets(nested));
  if (!value || typeof value !== 'object') return value;

  const redacted = {};
  for (const [key, nested] of Object.entries(value)) {
    if (isSecretShapedKey(key)) continue;
    if (key === 'credentialRef') {
      const credentialRef = redactCredentialReference(nested);
      if (credentialRef !== undefined) redacted[key] = credentialRef;
      continue;
    }
    if (key === 'instances' && Array.isArray(nested)) {
      redacted[key] = nested.map(instance => redactInstanceSecrets(instance));
      continue;
    }
    redacted[key] = redactSecrets(nested);
  }
  return redacted;
}

function hasLegacyInstanceSecrets(document) {
  return Array.isArray(document?.instances)
    && document.instances.some(instance => (
      instance
      && typeof instance === 'object'
      && (Object.prototype.hasOwnProperty.call(instance, 'password')
        || Object.prototype.hasOwnProperty.call(instance, 'clientSecret')
        || Object.prototype.hasOwnProperty.call(instance?.credentialRef || {}, 'passwordRef')
        || Object.prototype.hasOwnProperty.call(instance?.credentialRef || {}, 'clientSecretRef'))
    ));
}

function normalizeCredentialRefAliases(value, { allowAliases = false } = {}) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return value;
  const normalized = { ...value };
  for (const [canonical, alias] of [['password', 'passwordRef'], ['clientSecret', 'clientSecretRef']]) {
    if (!Object.prototype.hasOwnProperty.call(normalized, alias)) continue;
    if (Object.prototype.hasOwnProperty.call(normalized, canonical)) {
      if (normalized[canonical] !== normalized[alias]) {
        invalid(`Conflicting credential references '${canonical}' and '${alias}'`, { field: `credentialRef.${canonical}` });
      }
    } else if (!allowAliases) {
      invalid(`Credential reference alias '${alias}' is not accepted`, { field: `credentialRef.${alias}` });
    } else {
      normalized[canonical] = normalized[alias];
    }
    delete normalized[alias];
  }
  return normalized;
}

function normalizeInstanceCredentialRefs(instance, options) {
  if (!instance || typeof instance !== 'object' || Array.isArray(instance)) return instance;
  if (!Object.prototype.hasOwnProperty.call(instance, 'credentialRef')) return { ...instance };
  return {
    ...instance,
    credentialRef: normalizeCredentialRefAliases(instance.credentialRef, options)
  };
}

function invalid(message, details = {}) {
  throw new InstanceRegistryError('INVALID_INSTANCE_CONFIG', message, details);
}

export function canonicalizeInstanceUrl(url) {
  const parsed = new URL(url);
  const pathname = parsed.pathname.replace(/\/+$/, '');
  return `${parsed.origin}${pathname || ''}`;
}

function validateUrl(url, name) {
  if (typeof url !== 'string' || !url.trim()) {
    invalid(`Instance '${name || 'unknown'}' requires a URL`, { field: 'url' });
  }

  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    invalid(`Instance '${name || 'unknown'}' has an invalid URL`, { field: 'url' });
  }
  if (parsed.username || parsed.password) {
    invalid(`Instance '${name || 'unknown'}' URL must not include username or password`, { field: 'url' });
  }
  if (url.includes('?') || url.includes('#')) {
    invalid(`Instance '${name || 'unknown'}' URL must not include a query or fragment`, { field: 'url' });
  }

  if (parsed.protocol === 'https:') return;
  const loopbackHosts = new Set(['127.0.0.1', 'localhost', '::1']);
  const hostname = parsed.hostname.replace(/^\[/, '').replace(/\]$/, '').toLowerCase();
  if (parsed.protocol !== 'http:' || !loopbackHosts.has(hostname)) {
    invalid(`Instance '${name || 'unknown'}' must use HTTPS except for loopback HTTP`, { field: 'url' });
  }
}

function validateOptionalUrl(value, name, field) {
  if (typeof value !== 'string' || !value.trim()) {
    invalid(`Instance '${name}' ${field} must be a non-empty URL`, { field });
  }
  let parsed;
  try {
    parsed = new URL(value);
  } catch {
    invalid(`Instance '${name}' ${field} must be a valid URL`, { field });
  }
  if (parsed.username || parsed.password) {
    invalid(`Instance '${name}' ${field} URL must not include username or password`, { field });
  }
  const loopbackHosts = new Set(['127.0.0.1', 'localhost', '::1']);
  const hostname = parsed.hostname.replace(/^\[/, '').replace(/\]$/, '').toLowerCase();
  if (parsed.protocol !== 'https:' && !(parsed.protocol === 'http:' && loopbackHosts.has(hostname))) {
    invalid(`Instance '${name}' ${field} must use HTTPS except for loopback HTTP`, { field });
  }
}

function validateOptionalString(value, name, field) {
  if (typeof value !== 'string') {
    invalid(`Instance '${name}' ${field} must be a string`, { field });
  }
}

function validateNonEmptyString(value, name, field) {
  if (typeof value !== 'string' || !value.trim()) {
    invalid(`Instance '${name}' ${field} must be a non-empty string`, { field });
  }
}

function validateCallbackPath(value, name) {
  if (typeof value !== 'string' || !value.trim() || !value.startsWith('/') || value.startsWith('//') || value.includes('?') || value.includes('#')) {
    invalid(`Instance '${name}' callbackPath must be a valid absolute path`, { field: 'callbackPath' });
  }
}

function canonicalCredentialRef(value, name, type, field = 'credentialRef') {
  if (typeof value !== 'string' || !value.trim()) {
    invalid(`Instance credential reference '${field}' must be a non-empty canonical reference`, { field });
  }
  let parsed;
  try {
    parsed = parseCredentialRef(value);
  } catch {
    invalid(`Instance credential reference '${field}' is invalid`, { field });
  }
  const expected = credentialRefFor(name, type);
  if (parsed.ref !== expected || parsed.instanceName !== name || parsed.type !== type) {
    invalid(`Instance credential reference '${field}' does not match the instance`, { field });
  }
  return parsed.ref;
}

function passwordGrantRefs(value, name, { allowAliases = false } = {}) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    invalid("OAuth password grant requires credentialRef.password and credentialRef.clientSecret", { field: 'credentialRef' });
  }
  const normalized = normalizeCredentialRefAliases(value, { allowAliases });
  for (const key of Object.keys(normalized)) {
    if (!CANONICAL_CREDENTIAL_FIELDS.has(key)) {
      invalid(`Unknown credential reference field '${key}'`, { field: 'credentialRef' });
    }
  }
  return {
    password: canonicalCredentialRef(normalized.password, name, 'password', 'credentialRef.password'),
    clientSecret: canonicalCredentialRef(normalized.clientSecret, name, 'client-secret', 'credentialRef.clientSecret')
  };
}

function validateNewInstance(instance, { allowSecrets = false } = {}) {
  if (!instance || typeof instance !== 'object' || Array.isArray(instance)) {
    invalid('Instance configuration must be an object');
  }
  const strictBranches = !allowSecrets;
  for (const field of Object.keys(instance)) {
    if (!ALLOWED_FIELDS.has(field) && !(allowSecrets && LEGACY_INSTANCE_SECRET_FIELDS.has(field))) {
      invalid(`Unknown instance field '${field}'`, { field });
    }
  }

  const name = typeof instance.name === 'string' ? instance.name.trim() : '';
  if (!name || !NAME_PATTERN.test(name) || name !== instance.name) {
    invalid('Instance name must be a trimmed identifier containing only letters, numbers, underscores, and hyphens', { field: 'name' });
  }
  validateUrl(instance.url, name);
  const authType = instance.authType === undefined ? 'basic' : instance.authType;
  if (!AUTH_TYPES.has(authType)) invalid(`Instance '${name}' has an unsupported authType`, { field: 'authType' });
  if (instance.default !== undefined && typeof instance.default !== 'boolean') invalid(`Instance '${name}' default must be a boolean`, { field: 'default' });
  if (instance.scope !== undefined) validateOptionalString(instance.scope, name, 'scope');
  if (instance.description !== undefined) validateOptionalString(instance.description, name, 'description');
  if (instance.authorizeUrl !== undefined) validateOptionalUrl(instance.authorizeUrl, name, 'authorizeUrl');
  if (instance.tokenUrl !== undefined) validateOptionalUrl(instance.tokenUrl, name, 'tokenUrl');
  for (const field of ['authorizeUrl', 'tokenUrl']) {
    if (instance[field] === undefined) continue;
    try {
      assertApprovedOAuthEndpoint(instance[field], instance.url, field);
    } catch (error) {
      invalid(`Instance '${name}' ${error.message}`, { field });
    }
  }
  if (instance.callbackPath !== undefined) validateCallbackPath(instance.callbackPath, name);
  if (instance.redirectPort !== undefined && (!Number.isInteger(instance.redirectPort) || instance.redirectPort < 0 || instance.redirectPort > 65535)) {
    invalid(`Instance '${name}' redirectPort must be an integer from 0 to 65535`, { field: 'redirectPort' });
  }
  if (instance.username !== undefined) validateNonEmptyString(instance.username, name, 'username');
  if (instance.clientId !== undefined) validateNonEmptyString(instance.clientId, name, 'clientId');
  if (allowSecrets && instance.password !== undefined) validateNonEmptyString(instance.password, name, 'password');
  if (allowSecrets && instance.clientSecret !== undefined) validateNonEmptyString(instance.clientSecret, name, 'clientSecret');

  if (authType === 'basic') {
    if (instance.grantType !== undefined) invalid(`Basic instance '${name}' cannot specify grantType`, { field: 'grantType' });
    if (strictBranches) {
      for (const field of ['clientId', 'scope', 'authorizeUrl', 'tokenUrl', 'redirectPort', 'callbackPath']) {
        if (instance[field] !== undefined) invalid(`Basic instance '${name}' cannot specify ${field}`, { field });
      }
    }
    if (typeof instance.username !== 'string' || !instance.username.trim()) invalid(`Basic instance '${name}' requires username as a non-empty string`, { field: 'username' });
    if (Object.prototype.hasOwnProperty.call(instance, 'credentialRef')) canonicalCredentialRef(instance.credentialRef, name, 'password');
    if (allowSecrets && instance.password) return true;
    canonicalCredentialRef(instance.credentialRef, name, 'password');
    return true;
  }

  const grantType = instance.grantType === undefined
    ? (instance.username ? 'password' : 'client_credentials')
    : instance.grantType;
  if (!GRANT_TYPES.has(grantType)) invalid(`OAuth instance '${name}' has an unsupported grantType`, { field: 'grantType' });
  if (typeof instance.clientId !== 'string' || !instance.clientId.trim()) invalid(`OAuth instance '${name}' requires clientId as a non-empty string`, { field: 'clientId' });

  if (grantType === 'authorization_code') {
    if (strictBranches && instance.username !== undefined) invalid(`Authorization-code instance '${name}' cannot specify username`, { field: 'username' });
    if (instance.credentialRef !== undefined) invalid(`Public authorization-code instance '${name}' must not specify credentialRef`, { field: 'credentialRef' });
    return true;
  }

  if (grantType === 'client_credentials') {
    if (strictBranches) {
      for (const field of ['username', 'authorizeUrl', 'redirectPort', 'callbackPath']) {
        if (instance[field] !== undefined) invalid(`Client-credentials instance '${name}' cannot specify ${field}`, { field });
      }
    }
    if (Object.prototype.hasOwnProperty.call(instance, 'credentialRef')) canonicalCredentialRef(instance.credentialRef, name, 'client-secret');
    if (allowSecrets && instance.clientSecret) return true;
    canonicalCredentialRef(instance.credentialRef, name, 'client-secret');
    return true;
  }

  if (strictBranches) {
    for (const field of ['authorizeUrl', 'redirectPort', 'callbackPath']) {
      if (instance[field] !== undefined) invalid(`Password-grant instance '${name}' cannot specify ${field}`, { field });
    }
  }
  if (typeof instance.username !== 'string' || !instance.username.trim()) invalid(`OAuth password instance '${name}' requires username as a non-empty string`, { field: 'username' });
  if (Object.prototype.hasOwnProperty.call(instance, 'credentialRef')) passwordGrantRefs(instance.credentialRef, name, { allowAliases: allowSecrets });
  if (allowSecrets && instance.password && instance.clientSecret) return true;
  passwordGrantRefs(instance.credentialRef, name, { allowAliases: allowSecrets });
  return true;
}

function validateDocument(document) {
  if (!document || typeof document !== 'object' || Array.isArray(document)) {
    throw new InstanceRegistryError('REGISTRY_RELOAD_FAILED', 'Instance registry document must be an object');
  }
  if (document.version !== undefined && document.version !== 1) {
    throw new InstanceRegistryError('REGISTRY_RELOAD_FAILED', 'Unsupported instance registry version', { version: document.version });
  }
  if (!Array.isArray(document.instances)) {
    throw new InstanceRegistryError('REGISTRY_RELOAD_FAILED', 'Instance registry instances must be an array');
  }
  const legacyPlaintext = hasLegacyInstanceSecrets(document);
  const normalizedDocument = {
    ...document,
    instances: document.instances.map(instance => normalizeInstanceCredentialRefs(instance, { allowAliases: legacyPlaintext }))
  };
  const seen = new Set();
  let defaultCount = 0;
  for (const instance of normalizedDocument.instances) {
    const name = instance && instance.name;
    if (seen.has(name)) {
      throw new InstanceRegistryError('REGISTRY_RELOAD_FAILED', `Duplicate instance name '${name}' in registry`, { field: 'name' });
    }
    seen.add(name);
    if (instance && instance.default === true) {
      defaultCount += 1;
      if (defaultCount > 1) {
        throw new InstanceRegistryError('REGISTRY_RELOAD_FAILED', 'Instance registry may contain at most one default instance', { field: 'default' });
      }
    }
    validateNewInstance(instance, { allowSecrets: legacyPlaintext && hasLegacyInstanceSecrets({ instances: [instance] }) });
  }
  return normalizedDocument;
}

export class InstanceRegistryError extends Error {
  constructor(code, message, details = {}) {
    super(message);
    this.name = 'InstanceRegistryError';
    this.code = code;
    this.details = details;
  }
}

export class InstanceRegistry {
  constructor(options = {}) {
    const suppliedRegistry = options && typeof options.load === 'function' ? options : null;
    const config = suppliedRegistry ? {} : options;
    const hasExplicitPaths = config.readPath && config.writePath;

    this._pathOptions = hasExplicitPaths ? null : {
      env: config.env,
      homeDir: config.homeDir || os.homedir(),
      legacyPath: config.legacyPath || DEFAULT_LEGACY_PATH,
      existsSync: config.existsSync || fs.existsSync
    };
    this._pathsResolved = false;
    this.fs = config.fs || fs;
    this._document = null;
    this._loaded = false;
    this._legacyPlaintext = false;

    if (hasExplicitPaths) {
      this.readPath = path.resolve(config.readPath);
      this.writePath = path.resolve(config.writePath);
      this.source = 'explicit';
      this._pathsResolved = true;
    }
  }

  _resolvePaths() {
    if (this._pathsResolved) return;

    const paths = resolveConfigPaths({
      env: this._pathOptions.env || process.env,
      homeDir: this._pathOptions.homeDir,
      legacyPath: this._pathOptions.legacyPath,
      existsSync: this._pathOptions.existsSync
    });
    this.readPath = path.resolve(paths.readPath);
    this.writePath = path.resolve(paths.writePath);
    this.source = paths.source;
    this._pathsResolved = true;
  }

  get document() {
    return this.load();
  }

  hasFile() {
    this._resolvePaths();
    return this.fs.existsSync(this.readPath);
  }

  _readDocument(filePath, redact = true) {
    let document;
    try {
      document = JSON.parse(this.fs.readFileSync(filePath, 'utf8'));
    } catch (error) {
      if (error.code === 'ENOENT') {
        document = { version: 1, instances: [] };
      } else if (error instanceof SyntaxError) {
        throw new InstanceRegistryError('REGISTRY_RELOAD_FAILED', 'Failed to parse instance registry JSON', { path: filePath });
      } else {
        throw new InstanceRegistryError('REGISTRY_RELOAD_FAILED', `Failed to load instance registry: ${error.message}`, { path: filePath });
      }
    }

    try {
      document = validateDocument(document);
    } catch (error) {
      const details = error instanceof InstanceRegistryError ? error.details : {};
      const message = error instanceof Error ? error.message : 'Invalid instance registry document';
      throw new InstanceRegistryError('REGISTRY_RELOAD_FAILED', message, {
        ...details,
        path: filePath
      });
    }
    document = {
      ...document,
      instances: document.instances.map(instance => ({
        ...instance,
        url: canonicalizeInstanceUrl(instance.url)
      }))
    };
    if (document.version === undefined) document = { ...document, version: 1 };
    this._document = document;
    this._legacyPlaintext = hasLegacyInstanceSecrets(document);
    this._loaded = true;
    return redact ? redactSecrets(this._document) : this._document;
  }

  _loadForMutation() {
    const sourcePath = this.fs.existsSync(this.writePath) ? this.writePath : this.readPath;
    return this._readDocument(sourcePath, false);
  }

  load() {
    this._resolvePaths();
    if (this._loaded) return redactSecrets(this._document);
    return this._readDocument(this.readPath);
  }

  reload() {
    const previous = this._document;
    const previousLoaded = this._loaded;
    const previousLegacy = this._legacyPlaintext;
    this._loaded = false;
    try {
      return this.load();
    } catch (error) {
      this._document = previous;
      this._loaded = previousLoaded;
      this._legacyPlaintext = previousLegacy;
      if (error instanceof InstanceRegistryError && error.code === 'REGISTRY_RELOAD_FAILED') throw error;
      throw new InstanceRegistryError('REGISTRY_RELOAD_FAILED', `Failed to reload instance registry: ${error.message}`, { path: this.readPath });
    }
  }

  _redactInstance(instance) {
    return redactSecrets(instance);
  }

  _rawDocument() {
    this.load();
    return this._document;
  }

  _listForClient() {
    return clone(this._rawDocument().instances);
  }

  _getForClient(name) {
    const instance = this._rawDocument().instances.find(candidate => candidate.name === name);
    if (!instance) {
      throw new InstanceRegistryError('INSTANCE_NOT_FOUND', `Instance '${name}' not found`, { name });
    }
    return clone(instance);
  }

  _getDefaultForClient() {
    const instances = this._rawDocument().instances;
    return clone(instances.find(instance => instance.default === true) || instances[0]);
  }

  list() {
    return this.load().instances.map(instance => this._redactInstance(instance));
  }

  get(name) {
    const instance = this.load().instances.find(candidate => candidate.name === name);
    if (!instance) {
      throw new InstanceRegistryError('INSTANCE_NOT_FOUND', `Instance '${name}' not found`, { name });
    }
    return this._redactInstance(instance);
  }

  getDefault() {
    const instances = this.load().instances;
    return this._redactInstance(instances.find(instance => instance.default === true) || instances[0]);
  }

  validate(instance) {
    return validateNewInstance(instance, { allowSecrets: true });
  }

  validateDocument(document) {
    return validateDocument(document);
  }

  register(instance, { makeDefault = false, precommit, captureContext } = {}) {
    if (typeof makeDefault !== 'boolean') {
      invalid('register options makeDefault must be a boolean', { field: 'makeDefault' });
    }
    if (precommit !== undefined && typeof precommit !== 'function') {
      invalid('register options precommit must be a function', { field: 'precommit' });
    }
    if (captureContext !== undefined && typeof captureContext !== 'function') {
      invalid('register options captureContext must be a function', { field: 'captureContext' });
    }
    return this._enqueueMutation(() => {
      const canonicalInstance = normalizeInstanceCredentialRefs(instance);
      validateNewInstance(canonicalInstance);
      const current = this._document.instances;
      if (current.some(candidate => candidate.name === canonicalInstance.name)) {
        throw new InstanceRegistryError('INSTANCE_ALREADY_EXISTS', `Instance '${canonicalInstance.name}' already exists`, { name: canonicalInstance.name });
      }
      const shouldDefault = current.length === 0 || makeDefault || canonicalInstance.default === true;
      const candidate = {
        ...clone(canonicalInstance),
        url: canonicalizeInstanceUrl(canonicalInstance.url),
        authType: canonicalInstance.authType === undefined ? 'basic' : canonicalInstance.authType,
        default: shouldDefault
      };
      const instances = current.map(existing => shouldDefault ? { ...existing, default: false } : existing);
      instances.push(candidate);
      return { ...this._document, version: 1, instances };
    }, updated => updated.instances.find(candidate => candidate.name === instance.name), precommit, captureContext);
  }

  update(name, patch) {
    return this._enqueueMutation(() => {
      const current = this._document.instances.find(instance => instance.name === name);
      if (!current) throw new InstanceRegistryError('INSTANCE_NOT_FOUND', `Instance '${name}' not found`, { name });
      const merged = normalizeInstanceCredentialRefs({ ...current, ...clone(patch), name });
      validateNewInstance(merged);
      merged.url = canonicalizeInstanceUrl(merged.url);
      const shouldDefault = merged.default === true;
      const instances = this._document.instances.map(instance => {
        if (instance.name === name) return { ...merged, default: shouldDefault };
        return shouldDefault ? { ...instance, default: false } : instance;
      });
      return { ...this._document, version: 1, instances };
    }, updated => updated.instances.find(candidate => candidate.name === name));
  }

  compensateRegistration(name, { expected, priorDefault } = {}) {
    return this._enqueueMutation(() => {
      const current = this._document.instances.find(instance => instance.name === name);
      if (!current || expected === undefined || JSON.stringify(current) !== JSON.stringify(expected)) {
        throw new InstanceRegistryError(
          'REGISTRY_ROLLBACK_REQUIRED',
          `Instance '${name}' changed before registration compensation`,
          { name, rollbackRequired: true }
        );
      }

      const instances = this._document.instances
        .filter(instance => instance.name !== name)
        .map(instance => {
          if (!priorDefault || instance.name !== priorDefault.name) return instance;
          return { ...instance, default: priorDefault.default };
        });
      const nextDocument = {
        ...this._document,
        version: 1,
        instances
      };
      validateDocument(nextDocument);
      return nextDocument;
    }, () => undefined);
  }

  remove(name, { expected } = {}) {
    return this._enqueueMutation(() => {
      const current = this._document.instances.find(instance => instance.name === name);
      if (!current) {
        throw new InstanceRegistryError('INSTANCE_NOT_FOUND', `Instance '${name}' not found`, { name });
      }
      if (expected !== undefined && JSON.stringify(current) !== JSON.stringify(expected)) {
        throw new InstanceRegistryError('INSTANCE_CHANGED', `Instance '${name}' changed before compensation`, { name });
      }
      return {
        ...this._document,
        version: 1,
        instances: this._document.instances.filter(instance => instance.name !== name)
      };
    }, () => undefined);
  }
  _enqueueMutation(operation, resultSelector, precommit, captureContext) {
    this._resolvePaths();
    const writePath = this.writePath;
    return enqueuePathMutation(writePath, async () => {
      this._loadForMutation();
      if (this._legacyPlaintext) {
        throw new InstanceRegistryError(
          'LEGACY_MIGRATION_REQUIRED',
          'This registry contains plaintext credentials; migrate it before changing instances',
          { path: this.readPath }
        );
      }

      const nextDocument = operation();
      validateDocument(nextDocument);
      if (captureContext) {
        try {
          const result = captureContext(clone(this._document), clone(nextDocument));
          if (result && typeof result.then === 'function') {
            throw new Error('register captureContext must be synchronous');
          }
        } catch {
          throw new InstanceRegistryError('REGISTRY_CONTEXT_FAILED', 'Registry mutation context could not be captured');
        }
      }
      if (precommit) {
        await precommit(clone(nextDocument));
      }
      await this._writeAtomic(nextDocument);
      this._document = nextDocument;
      this._loaded = true;
      this._legacyPlaintext = false;
      if (this.readPath !== this.writePath) {
        this.readPath = this.writePath;
        this.source = 'user';
      }
      return clone(resultSelector(nextDocument));
    });
  }

  async _writeAtomic(document) {
    const directory = path.dirname(this.writePath);
    const basename = path.basename(this.writePath);
    let tempPath;
    let handle;
    try {
      await this.fs.promises.mkdir(directory, { recursive: true, mode: 0o700 });
      for (let attempt = 0; attempt < 10; attempt += 1) {
        const candidatePath = path.join(directory, `.${basename}.${process.pid}.${Date.now()}.${Math.random().toString(16).slice(2)}.${attempt}.tmp`);
        try {
          handle = await this.fs.promises.open(candidatePath, 'wx', 0o600);
          tempPath = candidatePath;
          break;
        } catch (error) {
          if (error.code !== 'EEXIST' || attempt === 9) throw error;
        }
      }
      await handle.writeFile(`${JSON.stringify(document, null, 2)}\n`, 'utf8');
      await handle.close();
      handle = undefined;
      await this.fs.promises.rename(tempPath, this.writePath);
    } catch (error) {
      if (handle) {
        try { await handle.close(); } catch { /* cleanup best effort */ }
      }
      if (tempPath) {
        try { await this.fs.promises.unlink(tempPath); } catch { /* cleanup best effort */ }
      }
      if (error instanceof InstanceRegistryError) throw error;
      throw new InstanceRegistryError('REGISTRY_WRITE_FAILED', 'Failed to write instance registry', { path: path.resolve(this.writePath) });
    }
  }
}
