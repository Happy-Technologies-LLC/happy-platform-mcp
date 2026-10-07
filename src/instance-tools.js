import path from 'node:path';
import { credentialRefFor } from './instance-credential-store.js';
import { MAX_URL_LENGTH } from './url-limits.js';

const TOOL_NAME = 'SN-Register-Instance';
const ALLOWED_FIELDS = new Set([
  'name',
  'url',
  'authType',
  'grantType',
  'username',
  'clientId',
  'scope',
  'authorizeUrl',
  'tokenUrl',
  'redirectPort',
  'callbackPath',
  'description',
  'makeDefault'
]);
const OUTPUT_FIELDS = ALLOWED_FIELDS;
const SECRET_KEY_PATTERN = /(?:password|secret|token|credential|api[_-]?key|private[_-]?key)/i;
const SAFE_CREDENTIAL_ERROR_MESSAGES = new Map([
  ['CREDENTIAL_NOT_FOUND', 'Required credential is missing; store it with the local credential command and retry.'],
  ['KEYCHAIN_UNAVAILABLE', 'Credential store unavailable; unlock or configure the local keychain and retry.'],
  ['KEYCHAIN_OPERATION_FAILED', 'Credential store operation failed; verify local keychain access and retry.']
]);

const INSTANCE_SCHEMA_PROPERTIES = {
  name: { type: 'string', description: 'Unique local instance name (for example, dev or prod).' },
  url: { type: 'string', maxLength: MAX_URL_LENGTH, description: 'HTTPS ServiceNow instance URL.' },
  authType: { type: 'string', enum: ['basic', 'oauth'], description: 'Authentication type. Defaults to basic.' },
  grantType: { type: 'string', enum: ['client_credentials', 'password', 'authorization_code'], description: 'OAuth grant type.' },
  username: { type: 'string', description: 'Basic-auth or OAuth password-grant username.' },
  clientId: { type: 'string', description: 'OAuth client identifier.' },
  scope: { type: 'string', description: 'Optional OAuth scope.' },
  authorizeUrl: { type: 'string', maxLength: MAX_URL_LENGTH, description: 'Optional OAuth authorization endpoint.' },
  tokenUrl: { type: 'string', maxLength: MAX_URL_LENGTH, description: 'Optional OAuth token endpoint.' },
  redirectPort: { type: 'integer', minimum: 0, maximum: 65535, description: 'Optional local authorization callback port.' },
  callbackPath: { type: 'string', maxLength: MAX_URL_LENGTH, description: 'Optional local authorization callback path.' },
  description: { type: 'string', description: 'Optional human-readable description.' },
  makeDefault: { type: 'boolean', description: 'Make this instance the default after registration.' }
};

const schemaBranch = (fields, required, authType, grantType) => ({
  type: 'object',
  additionalProperties: false,
  properties: Object.fromEntries(fields.map(field => [
    field,
    field === 'authType' && authType ? { ...INSTANCE_SCHEMA_PROPERTIES.authType, const: authType } :
      field === 'grantType' && grantType ? { ...INSTANCE_SCHEMA_PROPERTIES.grantType, const: grantType } :
        INSTANCE_SCHEMA_PROPERTIES[field]
  ])),
  required
});

const INSTANCE_SCHEMA_BRANCHES = [
  schemaBranch(['name', 'url', 'authType', 'username', 'description', 'makeDefault'], ['name', 'url', 'username'], 'basic'),
  schemaBranch(['name', 'url', 'authType', 'grantType', 'clientId', 'scope', 'tokenUrl', 'description', 'makeDefault'], ['name', 'url', 'authType', 'grantType', 'clientId'], 'oauth', 'client_credentials'),
  schemaBranch(['name', 'url', 'authType', 'grantType', 'username', 'clientId', 'scope', 'tokenUrl', 'description', 'makeDefault'], ['name', 'url', 'authType', 'grantType', 'username', 'clientId'], 'oauth', 'password'),
  schemaBranch(['name', 'url', 'authType', 'grantType', 'clientId', 'scope', 'authorizeUrl', 'tokenUrl', 'redirectPort', 'callbackPath', 'description', 'makeDefault'], ['name', 'url', 'authType', 'grantType', 'clientId'], 'oauth', 'authorization_code')
];

export const instanceToolDefinitions = [
  {
    name: TOOL_NAME,
    description: 'Register non-secret ServiceNow instance metadata using credentials already stored by the local CLI.',
    inputSchema: {
      type: 'object',
      additionalProperties: false,
      properties: INSTANCE_SCHEMA_PROPERTIES,
      oneOf: INSTANCE_SCHEMA_BRANCHES
    }
  }
];

export function isInstanceSetupTool(name) {
  return name === TOOL_NAME;
}

function response(payload, isError = false) {
  return {
    content: [{
      type: 'text',
      text: JSON.stringify(payload, null, 2)
    }],
    ...(isError ? { isError: true } : {})
  };
}

function errorResponse(code, message, details = {}) {
  return response({ success: false, code, message, ...details }, true);
}

function invalid(message = 'Invalid instance metadata') {
  return errorResponse('INVALID_INSTANCE_CONFIG', message);
}

function assertPlainObject(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    return invalid('Instance metadata must be an object');
  }
  return null;
}

function validateInputKeys(value, depth = 0) {
  if (!value || typeof value !== 'object') return null;
  if (Array.isArray(value)) {
    for (const item of value) {
      const nestedError = validateInputKeys(item, depth + 1);
      if (nestedError) return nestedError;
    }
    return null;
  }

  for (const [key, nested] of Object.entries(value)) {
    const knownTopLevelField = depth === 0 && ALLOWED_FIELDS.has(key);
    if (!knownTopLevelField && SECRET_KEY_PATTERN.test(key)) {
      return invalid('Secret-shaped fields are not accepted by this tool; use the local credential command');
    }
    if (depth === 0 && !knownTopLevelField) {
      return invalid('Unknown instance metadata field');
    }
    if (depth > 0) {
      return invalid('Nested instance metadata is not supported');
    }
    const nestedError = validateInputKeys(nested, depth + 1);
    if (nestedError) return nestedError;
  }
  return null;
}


function canonicalMetadata(input) {
  const authType = input.authType === undefined ? 'basic' : input.authType;
  const grantType = authType === 'oauth'
    ? (input.grantType === undefined
      ? (input.username === undefined ? 'client_credentials' : 'password')
      : input.grantType)
    : undefined;
  const metadata = {
    name: input.name,
    url: input.url,
    authType,
    ...(grantType === undefined ? {} : { grantType }),
    ...(input.username === undefined ? {} : { username: input.username }),
    ...(input.clientId === undefined ? {} : { clientId: input.clientId }),
    ...(input.scope === undefined ? {} : { scope: input.scope }),
    ...(input.authorizeUrl === undefined ? {} : { authorizeUrl: input.authorizeUrl }),
    ...(input.tokenUrl === undefined ? {} : { tokenUrl: input.tokenUrl }),
    ...(input.redirectPort === undefined ? {} : { redirectPort: input.redirectPort }),
    ...(input.callbackPath === undefined ? {} : { callbackPath: input.callbackPath }),
    ...(input.description === undefined ? {} : { description: input.description })
  };

  if (authType === 'basic') {
    metadata.credentialRef = credentialRefFor(input.name, 'password');
  } else if (grantType === 'client_credentials') {
    metadata.credentialRef = credentialRefFor(input.name, 'client-secret');
  } else if (grantType === 'password') {
    metadata.credentialRef = {
      password: credentialRefFor(input.name, 'password'),
      clientSecret: credentialRefFor(input.name, 'client-secret')
    };
  }
  return { metadata, authType, grantType };
}

function outputMetadata(instance, input) {
  const output = {};
  for (const field of OUTPUT_FIELDS) {
    if (field === 'makeDefault') {
      if (instance?.default !== undefined) output.makeDefault = instance.default;
      else if (input?.makeDefault !== undefined) output.makeDefault = input.makeDefault;
      continue;
    }
    if (instance?.[field] !== undefined) output[field] = instance[field];
    else if (input?.[field] !== undefined) output[field] = input[field];
  }
  return output;
}

function credentialCommands(name, types) {
  return types.map(type => `happy-platform-mcp instance credential set ${name} --type ${type}`);
}

function credentialRequirements(authType, grantType, name) {
  if (authType === 'basic') return [{ ref: credentialRefFor(name, 'password'), type: 'password' }];
  if (grantType === 'client_credentials') {
    return [{ ref: credentialRefFor(name, 'client-secret'), type: 'client-secret' }];
  }
  if (grantType === 'password') {
    return [
      { ref: credentialRefFor(name, 'password'), type: 'password' },
      { ref: credentialRefFor(name, 'client-secret'), type: 'client-secret' }
    ];
  }
  return [];
}

async function existingInstance(registry, name) {
  if (typeof registry.get === 'function') {
    try {
      const found = await registry.get(name);
      if (found) return found;
    } catch (error) {
      if (error?.code !== 'INSTANCE_NOT_FOUND') throw error;
    }
  }
  if (typeof registry.list === 'function') {
    const instances = await registry.list();
    return instances.find(instance => instance?.name === name) || null;
  }
  return null;
}

function safeRegistryDetails(error) {
  const source = error?.details && typeof error.details === 'object' ? error.details : {};
  const details = {};
  if (typeof source.field === 'string' && /^[A-Za-z][A-Za-z0-9_.-]*$/.test(source.field)
      && !/(?:password|secret|token|private|api[_-]?key)/i.test(source.field)) {
    details.field = source.field;
  }
  if (typeof source.path === 'string' && source.path) {
    const resolved = path.resolve(source.path);
    if (!/(?:password|secret|token|private|api[_-]?key)/i.test(resolved)) details.path = resolved;
  }
  return details;
}

function registryError(error) {
  if (error?.code === 'INSTANCE_ALREADY_EXISTS') {
    return errorResponse('INSTANCE_ALREADY_EXISTS', 'An instance with this name already exists; use the local CLI update command.', {
      commands: [`happy-platform-mcp instance update ${error.details?.name || 'the-instance'}`]
    });
  }
  if (error?.code === 'INVALID_INSTANCE_CONFIG') {
    return errorResponse('INVALID_INSTANCE_CONFIG', 'Instance metadata failed canonical validation', {
      details: safeRegistryDetails(error)
    });
  }
  if (error?.code === 'CREDENTIAL_NOT_FOUND' || typeof error?.code === 'string' && error.code.startsWith('KEYCHAIN_')) {
    const requestedCode = typeof error?.code === 'string' ? error.code : '';
    const code = SAFE_CREDENTIAL_ERROR_MESSAGES.has(requestedCode)
      ? requestedCode
      : 'KEYCHAIN_OPERATION_FAILED';
    return errorResponse(code, SAFE_CREDENTIAL_ERROR_MESSAGES.get(code), safeRegistryDetails(error));
  }
  if (error?.code === 'REGISTRY_ROLLBACK_REQUIRED') {
    return errorResponse(
      'REGISTRY_ROLLBACK_REQUIRED',
      'The registration could not be rolled back safely because it changed concurrently; manual rollback is required.',
      { persisted: true, partial: true, rollbackRequired: true, restartRequired: true }
    );
  }
  if (error?.code === 'REGISTRY_RELOAD_FAILED') {
    const details = safeRegistryDetails(error);
    return errorResponse(
      'REGISTRY_RELOAD_FAILED',
      'The instance registry could not be loaded. Repair the registry file or restart the MCP server after fixing it.',
      details.path ? { path: details.path } : {}
    );
  }
  if (error?.code === 'REGISTRY_EMPTY') {
    const details = safeRegistryDetails(error);
    return errorResponse(
      'REGISTRY_EMPTY',
      'No registered instances are available. Register an instance with the local CLI, then restart the MCP server.',
      details.path ? { path: details.path } : {}
    );
  }
  if (error?.code === 'REGISTRY_WRITE_FAILED') {
    return errorResponse('REGISTRY_WRITE_FAILED', 'The instance registry could not be updated. No registration was committed.', {
      details: safeRegistryDetails(error)
    });
  }
  if (error?.code === 'LEGACY_MIGRATION_REQUIRED') {
    return errorResponse('LEGACY_MIGRATION_REQUIRED', 'Migrate the legacy instance registry with the local CLI before registering another instance.');
  }
  return errorResponse('INSTANCE_REGISTRATION_FAILED', 'The instance registration could not be completed.');
}

function logRegistrationDiagnostic(dependencies, operation, code, name) {
  const diagnostic = { operation, code };
  if (typeof name === 'string' && /^[A-Za-z0-9_-]+$/.test(name)) diagnostic.name = name;
  const logger = typeof dependencies?.diagnosticLogger === 'function'
    ? dependencies.diagnosticLogger
    : console.error;
  try {
    logger(JSON.stringify(diagnostic));
  } catch {
    // Diagnostics must never change registration or rollback behavior.
  }
}

async function rollbackRegistration({ registry, configManager, name, registered, priorDefault, input, dependencies }) {
  try {
    if (typeof registry.compensateRegistration !== 'function') {
      throw Object.assign(new Error('Registration compensation is unavailable'), {
        code: 'REGISTRY_ROLLBACK_REQUIRED'
      });
    }
    await registry.compensateRegistration(name, {
      expected: registered,
      priorDefault
    });
  } catch {
    logRegistrationDiagnostic(dependencies, 'registration_rollback', 'REGISTRY_ROLLBACK_REQUIRED', name);
    return registryError({ code: 'REGISTRY_ROLLBACK_REQUIRED' });
  }

  let restoreError;
  try {
    const restoreResult = await configManager.reload();
    if (restoreResult === false) {
      restoreError = new Error('Configuration restore did not complete');
    }
  } catch (error) {
    restoreError = error;
  }
  if (restoreError) {
    logRegistrationDiagnostic(dependencies, 'registration_restore_reload', 'REGISTRY_RELOAD_FAILED', name);
  }

  return errorResponse(
    'REGISTRY_RELOAD_FAILED',
    'Configuration reload failed; the persisted registration was rolled back.',
    {
      persisted: false,
      rolledBack: true,
      restartRequired: Boolean(restoreError),
      metadata: outputMetadata(registered, input)
    }
  );
}

/*
 * Keep this branch intentionally narrow: the public response describes only
 * whether persistence and rollback completed, never the underlying error.
 */
function registrationReloadFailure(dependencies, args, registry, configManager, registered, priorDefault) {
  return rollbackRegistration({
    registry,
    configManager,
    name: args.name,
    registered,
    priorDefault,
    input: args,
    dependencies
  });
}


async function checkCredentials(store, requirements, name) {
  if (requirements.length === 0) return null;
  if (!store || typeof store.hasSecret !== 'function') {
    return errorResponse('KEYCHAIN_UNAVAILABLE', 'The local credential store is unavailable; no registration was committed.');
  }

  const missing = [];
  for (const requirement of requirements) {
    try {
      const present = await store.hasSecret(requirement.ref);
      if (!present) missing.push(requirement.type);
    } catch (error) {
      if (error?.code === 'CREDENTIAL_NOT_FOUND') missing.push(requirement.type);
      else {
        const code = typeof error?.code === 'string' && error.code.startsWith('KEYCHAIN_')
          ? error.code
          : 'KEYCHAIN_UNAVAILABLE';
        return errorResponse(code, 'The local credential store could not be checked; no registration was committed.');
      }
    }
  }

  if (missing.length > 0) {
    return errorResponse('CREDENTIAL_NOT_FOUND', 'Store the required credential(s) locally, then retry registration.', {
      commands: credentialCommands(name, [...new Set(missing)])
    });
  }
  return null;
}
function credentialResponseError(result) {
  const payload = JSON.parse(result.content[0].text);
  const error = new Error(payload.message);
  error.code = payload.code;
  const { success: _success, code: _code, message: _message, ...details } = payload;
  error.details = details;
  return error;
}

export async function handleInstanceSetupTool(name, args = {}, dependencies = {}) {
  if (!isInstanceSetupTool(name)) {
    return errorResponse('UNKNOWN_TOOL', 'Unknown instance setup tool');
  }

  const shapeError = assertPlainObject(args) || validateInputKeys(args);
  if (shapeError) return shapeError;

  if (Object.prototype.hasOwnProperty.call(args, 'makeDefault') && typeof args.makeDefault !== 'boolean') {
    return invalid('makeDefault must be a boolean');
  }
  if ((args.authType === undefined || args.authType === 'basic') && args.grantType !== undefined) {
    return invalid('Basic authentication must not specify grantType');
  }

  let canonical;
  try {
    canonical = canonicalMetadata(args);
  } catch {
    return invalid('Instance metadata failed canonical validation');
  }

  const registry = dependencies.instanceRegistry || dependencies.registry;
  if (!registry || typeof registry.register !== 'function') {
    return errorResponse('REGISTRY_UNAVAILABLE', 'The instance registry is unavailable; no registration was committed.');
  }

  try {
    if (typeof registry.validate === 'function') {
      await registry.validate(canonical.metadata);
    }
  } catch (error) {
    return registryError(error);
  }

  try {
    if (await existingInstance(registry, args.name)) {
      return errorResponse('INSTANCE_ALREADY_EXISTS', 'An instance with this name already exists; use the local CLI update command.', {
        commands: [`happy-platform-mcp instance update ${args.name}`]
      });
    }
  } catch (error) {
    return registryError(error);
  }
  const requirements = credentialRequirements(canonical.authType, canonical.grantType, args.name);
  const credentialError = await checkCredentials(dependencies.credentialStore, requirements, args.name);
  if (credentialError) return credentialError;

  const configManager = dependencies.configManager;
  if (!configManager || typeof configManager.reload !== 'function') {
    return errorResponse('CONFIG_MANAGER_UNAVAILABLE', 'Configuration reload is unavailable; no registration was committed.');
  }
  let priorDefault;

  let registered;
  try {
    registered = await registry.register(canonical.metadata, {
      makeDefault: args.makeDefault === undefined ? false : args.makeDefault,
      captureContext: currentDocument => {
        const previous = currentDocument.instances.find(instance => instance.default === true);
        priorDefault = previous?.name
          ? { name: previous.name, default: previous.default }
          : undefined;
      },
      precommit: async () => {
        const finalCredentialError = await checkCredentials(dependencies.credentialStore, requirements, args.name);
        if (finalCredentialError) throw credentialResponseError(finalCredentialError);
      }
    });
  } catch (error) {
    return registryError(error);
  }

  const postCommitCredentialError = await checkCredentials(
    dependencies.credentialStore,
    requirements,
    args.name
  );
  if (postCommitCredentialError) {
    try {
      await registry.compensateRegistration(args.name, {
        expected: registered,
        priorDefault
      });
    } catch (error) {
      return registryError(error);
    }
    return postCommitCredentialError;
  }

  try {
    const reloadResult = await configManager.reload();
    if (reloadResult === false) {
      throw new Error('Configuration reload did not complete');
    }
    if (typeof dependencies.onConfigReload === 'function') {
      await dependencies.onConfigReload();
    }
  } catch {
    logRegistrationDiagnostic(dependencies, 'registration_reload', 'REGISTRY_RELOAD_FAILED', args.name);
    return registrationReloadFailure(
      dependencies,
      args,
      registry,
      configManager,
      registered,
      priorDefault
    );
  }

  const restartRequired = dependencies.docsOnly === true;
  return response({
    success: true,
    restartRequired,
    ...(restartRequired
      ? {
        message: 'Instance registered. Restart the MCP server to enable live ServiceNow tools.'
      }
      : {}),
    metadata: outputMetadata(registered, args)
  });
}

export const handler = handleInstanceSetupTool;
