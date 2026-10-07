import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, describe, expect, jest, test } from '@jest/globals';
import { createMcpServer } from '../src/mcp-server-consolidated.js';
import { credentialRefFor, InstanceCredentialStore } from '../src/instance-credential-store.js';
import { InstanceRegistry } from '../src/instance-registry.js';

const tempDirs = [];

function tempRegistryPath() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-mcp-registration-'));
  tempDirs.push(dir);
  return path.join(dir, 'instances.json');
}

afterEach(() => {
  for (const dir of tempDirs.splice(0)) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

function parseResponse(response) {
  return JSON.parse(response.content[0].text);
}

async function createHarness({ docsOnly = false, credentialStore, reload = true, createServiceNowClient } = {}) {
  const registry = new InstanceRegistry({
    readPath: tempRegistryPath(),
    writePath: tempRegistryPath()
  });
  // Keep the read and write paths identical so assertions observe the committed registry.
  registry.readPath = registry.writePath;
  const configManager = {
    registry,
    getInstance: jest.fn(name => registry.get(name)),
    listInstances: jest.fn(() => registry.list()),
    reload: jest.fn(() => {
      if (reload) return registry.reload();
      throw new Error('configuration reload failed with secret-value');
    })
  };
  const store = credentialStore || { hasSecret: jest.fn(async () => true) };
  const server = await createMcpServer({ setProgressCallback() {} }, {
    docsOnly,
    configManager,
    instanceRegistry: registry,
    credentialStore: store,
    ...(createServiceNowClient ? { createServiceNowClient } : {})
  });
  const listTools = server._requestHandlers.get('tools/list');
  const callTool = server._requestHandlers.get('tools/call');
  return {
    registry,
    configManager,
    credentialStore: store,
    listTools: () => listTools({ method: 'tools/list', params: {} }, {}),
    callTool: (name, args) => callTool({
      method: 'tools/call',
      params: { name, arguments: args }
    }, {})
  };
}

const publicMetadata = (name = 'dev') => ({
  name,
  url: `https://${name}.service-now.com`,
  authType: 'oauth',
  grantType: 'authorization_code',
  clientId: 'public-client',
  makeDefault: true
});

describe('SN-Register-Instance', () => {
  test('appears once in normal and docs-only tool lists with metadata-only schema', async () => {
    const normal = await createHarness();
    const docsOnly = await createHarness({ docsOnly: true });

    for (const tools of [
      (await normal.listTools()).tools,
      (await docsOnly.listTools()).tools
    ]) {
      const definitions = tools.filter(tool => tool.name === 'SN-Register-Instance');
      expect(definitions).toHaveLength(1);
      expect(definitions[0].inputSchema.additionalProperties).toBe(false);
      expect(Object.keys(definitions[0].inputSchema.properties).sort()).toEqual([
        'authType',
        'authorizeUrl',
        'callbackPath',
        'clientId',
        'description',
        'grantType',
        'makeDefault',
        'name',
        'redirectPort',
        'scope',
        'tokenUrl',
        'url',
        'username'
      ].sort());
      expect(definitions[0].inputSchema.properties).not.toHaveProperty('password');
      expect(definitions[0].inputSchema.properties).not.toHaveProperty('clientSecret');
    }
  });
  test('describes four branch-exclusive metadata schemas with no credential inputs', async () => {
    const { tools } = await (await createHarness()).listTools();
    const schema = tools.find(tool => tool.name === 'SN-Register-Instance').inputSchema;
    expect(schema.oneOf).toHaveLength(4);
    for (const branch of schema.oneOf) {
      expect(branch.additionalProperties).toBe(false);
      expect(Object.keys(branch.properties).join(',')).not.toMatch(/password|clientSecret|credentialRef|secret|tokenRef/i);
    }
    const branchFields = schema.oneOf.map(branch => Object.keys(branch.properties).sort());
    expect(branchFields).toEqual(expect.arrayContaining([
      expect.arrayContaining(['name', 'url', 'authType', 'username']),
      expect.arrayContaining(['name', 'url', 'authType', 'grantType', 'clientId', 'tokenUrl']),
      expect.arrayContaining(['name', 'url', 'authType', 'grantType', 'clientId', 'username', 'tokenUrl']),
      expect.arrayContaining(['name', 'url', 'authType', 'grantType', 'clientId', 'authorizeUrl', 'redirectPort', 'callbackPath'])
    ]));
  });

  test('returns only safe registry error details and resolves valid paths', async () => {
    const harness = await createHarness();
    jest.spyOn(harness.registry, 'validate').mockRejectedValue(Object.assign(new Error('fixture-secret message'), {
      code: 'INVALID_INSTANCE_CONFIG',
      details: { field: 'clientId', path: '/tmp/registry.json', leaked: 'fixture-secret' }
    }));
    const invalidResult = await harness.callTool('SN-Register-Instance', publicMetadata('safe-error'));
    const invalidPayload = parseResponse(invalidResult);
    expect(invalidPayload).toMatchObject({
      code: 'INVALID_INSTANCE_CONFIG',
      message: 'Instance metadata failed canonical validation',
      details: { field: 'clientId', path: path.resolve('/tmp/registry.json') }
    });
    expect(JSON.stringify(invalidPayload)).not.toContain('fixture-secret');

    harness.registry.validate.mockRestore();
    jest.spyOn(harness.registry, 'register').mockRejectedValue(Object.assign(new Error('fixture-secret write'), {
      code: 'REGISTRY_WRITE_FAILED',
      details: { path: '/tmp/registry.json', leaked: 'fixture-secret' }
    }));
    const writeResult = await harness.callTool('SN-Register-Instance', publicMetadata('write-error'));
    const writePayload = parseResponse(writeResult);
    expect(writePayload).toMatchObject({
      code: 'REGISTRY_WRITE_FAILED',
      message: 'The instance registry could not be updated. No registration was committed.',
      details: { path: path.resolve('/tmp/registry.json') }
    });
    expect(JSON.stringify(writePayload)).not.toContain('fixture-secret');
  });
  test('maps malformed registry reload failures to a safe actionable error', async () => {
    const harness = await createHarness();
    const secret = 'malformed-fixture-secret';
    fs.writeFileSync(harness.registry.readPath, `{"instances":"${secret}"}`, 'utf8');

    const result = await harness.callTool('SN-Register-Instance', publicMetadata('malformed'));
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({
      code: 'REGISTRY_RELOAD_FAILED',
      path: path.resolve(harness.registry.readPath)
    });
    expect(payload.message).toMatch(/repair|restart/i);
    expect(payload.message).not.toContain(secret);
    expect(JSON.stringify(payload)).not.toContain(secret);
  });
  test('guides users to register when the registry is empty', async () => {
    const harness = await createHarness();
    jest.spyOn(harness.registry, 'validate').mockRejectedValue(Object.assign(new Error('raw registry details'), {
      code: 'REGISTRY_EMPTY',
      details: { path: harness.registry.readPath }
    }));

    const result = await harness.callTool('SN-Register-Instance', publicMetadata('empty'));
    const payload = parseResponse(result);

    expect(payload).toMatchObject({
      code: 'REGISTRY_EMPTY',
      path: path.resolve(harness.registry.readPath)
    });
    expect(payload.message).toMatch(/register|add/i);
    expect(payload.message).not.toContain('raw registry details');
  });

  test('registers public authorization-code metadata and reloads normal configuration once', async () => {
    const harness = await createHarness();
    const result = await harness.callTool('SN-Register-Instance', publicMetadata());
    const payload = parseResponse(result);

    expect(payload).toMatchObject({ success: true, restartRequired: false });
    expect(payload.metadata).toMatchObject({ name: 'dev', clientId: 'public-client' });
    expect(harness.configManager.reload).toHaveBeenCalledTimes(1);
    expect(harness.registry.list()).toEqual([
      expect.objectContaining({ name: 'dev', default: true })
    ]);
    expect(JSON.stringify(result)).not.toMatch(/password|clientSecret|fixture-secret/i);
  });
  test('persists and returns an operator-approved external token URL without exposing secrets', async () => {
    const saved = process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS;
    process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS = 'https://oauth.example.com';
    try {
      const harness = await createHarness();
      const tokenUrl = 'https://oauth.example.com/custom/token';
      const result = await harness.callTool('SN-Register-Instance', {
        ...publicMetadata('custom-token'),
        tokenUrl
      });
      const payload = parseResponse(result);

      expect(payload.metadata.tokenUrl).toBe(tokenUrl);
      expect(harness.registry.get('custom-token').tokenUrl).toBe(tokenUrl);
      expect(JSON.stringify(result)).not.toMatch(/password|clientSecret|accessToken|tokenValue|fixture-secret/i);
    } finally {
      if (saved === undefined) delete process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS;
      else process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS = saved;
    }
  });

  test('rejects caller-supplied off-policy OAuth endpoints without registering anything', async () => {
    const saved = process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS;
    delete process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS;
    try {
      const harness = await createHarness();
      for (const [field, value] of [['tokenUrl', 'https://attacker.example/token'], ['authorizeUrl', 'https://attacker.example/authorize']]) {
        const result = await harness.callTool('SN-Register-Instance', { ...publicMetadata(`off-${field.toLowerCase()}`), [field]: value });
        expect(result.isError).toBe(true);
        expect(parseResponse(result)).toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
      }
      expect(harness.registry.list()).toEqual([]);
    } finally {
      if (saved !== undefined) process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS = saved;
    }
  });

  test('makes a newly registered named instance resolvable immediately', async () => {
    const routedClient = {
      currentInstanceName: 'dev',
      setProgressCallback: jest.fn(),
      getRecords: jest.fn(async () => [])
    };
    const createServiceNowClient = jest.fn(() => routedClient);
    const harness = await createHarness({ createServiceNowClient });

    await harness.callTool('SN-Register-Instance', publicMetadata());
    const result = await harness.callTool('SN-Query-Table', {
      instance: 'dev',
      table_name: 'incident'
    });

    expect(result.isError).not.toBe(true);
    expect(createServiceNowClient).toHaveBeenCalledWith(expect.objectContaining({ name: 'dev' }));
    expect(routedClient.getRecords).toHaveBeenCalledWith('incident', expect.any(Object));
  });

  test('allows docs-only registration but asks the process to restart', async () => {
    const harness = await createHarness({ docsOnly: true });
    const result = await harness.callTool('SN-Register-Instance', publicMetadata());
    const payload = parseResponse(result);

    expect(payload).toMatchObject({
      success: true,
      restartRequired: true,
      message: 'Instance registered. Restart the MCP server to enable live ServiceNow tools.'
    });
    expect(harness.configManager.reload).toHaveBeenCalledTimes(1);
  });

  test('dispatches setup before the docs-only rejection guard', async () => {
    const harness = await createHarness({ docsOnly: true });
    const result = await harness.callTool('SN-Set-Instance', { instance_name: 'dev' });
    expect(result.isError).toBe(true);
    expect(result.content[0].text).toContain('unavailable in docs-only mode');
  });

  test('sanitizes keychain registration failures before returning MCP responses', async () => {
    const fixture = 'keychain-secret-fixture';
    const harness = await createHarness();
    jest.spyOn(harness.registry, 'get').mockRejectedValueOnce(Object.assign(
      new Error(`keychain backend failure ${fixture}`),
      { code: 'KEYCHAIN_UNAVAILABLE', details: { secret: fixture, path: '/tmp/instances.json' } }
    ));

    const result = await harness.callTool('SN-Register-Instance', publicMetadata('keychain-error'));
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({
      success: false,
      code: 'KEYCHAIN_UNAVAILABLE',
      message: 'Credential store unavailable; unlock or configure the local keychain and retry.'
    });
    expect(payload.path).toBe(path.resolve('/tmp/instances.json'));
    expect(JSON.stringify(result)).not.toContain(fixture);
  });
  test.each([
    ['validate', 'validate'],
    ['register', 'register']
  ])('sanitizes secret-bearing %s errors', async (_phase, method) => {
    const fixture = `registration-${method}-secret`;
    const harness = await createHarness();
    const failure = Object.assign(new Error(`backend failure ${fixture}`), {
      code: 'KEYCHAIN_OPERATION_FAILED',
      details: { secret: fixture, path: `/tmp/${method}-registry.json` }
    });
    if (method === 'validate') {
      jest.spyOn(harness.registry, 'validate').mockRejectedValueOnce(failure);
    } else {
      jest.spyOn(harness.registry, 'register').mockRejectedValueOnce(failure);
    }

    const result = await harness.callTool('SN-Register-Instance', publicMetadata(`${method}-error`));
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({
      success: false,
      code: 'KEYCHAIN_OPERATION_FAILED',
      message: 'Credential store operation failed; verify local keychain access and retry.'
    });
    expect(JSON.stringify(result)).not.toContain(fixture);
  });


  test('requires an existing basic password credential without mutating the registry', async () => {
    const store = { hasSecret: jest.fn(async () => false) };
    const harness = await createHarness({ credentialStore: store });
    const result = await harness.callTool('SN-Register-Instance', {
      name: 'dev',
      url: 'https://dev.service-now.com',
      authType: 'basic',
      username: 'developer'
    });
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({
      success: false,
      code: 'CREDENTIAL_NOT_FOUND',
      commands: ['happy-platform-mcp instance credential set dev --type password']
    });
    expect(harness.registry.list()).toEqual([]);
    expect(harness.configManager.reload).not.toHaveBeenCalled();
    expect(store.hasSecret).toHaveBeenCalledWith(credentialRefFor('dev', 'password'));
  });

  test('rejects a whitespace-only keychain credential without persisting metadata', async () => {
    const credentialStore = new InstanceCredentialStore({
      createEntry: async () => ({
        getPassword: async () => '   ',
        setPassword: async () => {},
        deletePassword: async () => true
      })
    });
    const harness = await createHarness({ credentialStore });

    const result = await harness.callTool('SN-Register-Instance', {
      name: 'blank-credential',
      url: 'https://blank-credential.service-now.com',
      authType: 'basic',
      username: 'user'
    });
    const payload = parseResponse(result);

    expect(payload).toMatchObject({ success: false, code: 'CREDENTIAL_NOT_FOUND' });
    expect(harness.registry.list()).toEqual([]);
    expect(harness.configManager.reload).not.toHaveBeenCalled();
  });

  test.each([
    ['client_credentials', 'client-secret'],
    ['password', 'password']
  ])('requires existing %s credentials', async (grantType, type) => {
    const store = { hasSecret: jest.fn(async () => false) };
    const harness = await createHarness({ credentialStore: store });
    const metadata = {
      name: grantType === 'password' ? 'password-grant' : 'client-grant',
      url: 'https://oauth.service-now.com',
      authType: 'oauth',
      grantType,
      clientId: 'client-id',
      ...(grantType === 'password' ? { username: 'user' } : {})
    };
    const result = await harness.callTool('SN-Register-Instance', metadata);
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload.code).toBe('CREDENTIAL_NOT_FOUND');
    if (grantType === 'client_credentials') {
      expect(payload.commands).toEqual([
        'happy-platform-mcp instance credential set client-grant --type client-secret'
      ]);
    } else {
      expect(payload.commands).toEqual([
        'happy-platform-mcp instance credential set password-grant --type password',
        'happy-platform-mcp instance credential set password-grant --type client-secret'
      ]);
    }
    expect(harness.registry.list()).toEqual([]);
    expect(type).toBeTruthy();
  });

  test('attaches deterministic existing credential references for all credentialed grants', async () => {
    const store = { hasSecret: jest.fn(async () => true) };
    const harness = await createHarness({ credentialStore: store });
    await harness.callTool('SN-Register-Instance', {
      name: 'basic',
      url: 'https://basic.service-now.com',
      authType: 'basic',
      username: 'user'
    });
    await harness.callTool('SN-Register-Instance', {
      name: 'client',
      url: 'https://client.service-now.com',
      authType: 'oauth',
      grantType: 'client_credentials',
      clientId: 'client-id'
    });
    await harness.callTool('SN-Register-Instance', {
      name: 'password',
      url: 'https://password.service-now.com',
      authType: 'oauth',
      grantType: 'password',
      clientId: 'client-id',
      username: 'user'
    });

    expect(harness.registry.get('basic').credentialRef)
      .toBe(credentialRefFor('basic', 'password'));
    expect(harness.registry.get('client').credentialRef)
      .toBe(credentialRefFor('client', 'client-secret'));
    expect(harness.registry.get('password').credentialRef).toEqual({
      password: credentialRefFor('password', 'password'),
      clientSecret: credentialRefFor('password', 'client-secret')
    });
    expect(harness.configManager.reload).toHaveBeenCalledTimes(3);
  });

  test('rejects secret-shaped and unknown nested keys when schema validation is bypassed', async () => {
    const harness = await createHarness();
    for (const args of [
      { ...publicMetadata('secret-key'), password: 'fixture-secret' },
      { ...publicMetadata('client-secret-key'), clientSecret: 'fixture-secret' },
      { ...publicMetadata('access-token-key'), accessToken: 'fixture-token' },
      { ...publicMetadata('token-value-key'), tokenValue: 'fixture-token' },
      { ...publicMetadata('nested-key'), nested: { clientSecret: 'fixture-secret' } }
    ]) {
      const result = await harness.callTool('SN-Register-Instance', args);
      const payload = parseResponse(result);
      expect(result.isError).toBe(true);
      expect(payload.code).toBe('INVALID_INSTANCE_CONFIG');
      expect(JSON.stringify(result)).not.toContain('fixture-secret');
    }
    expect(harness.registry.list()).toEqual([]);
    expect(harness.configManager.reload).not.toHaveBeenCalled();
  });

  test('rejects a grant type on basic authentication before credential lookup', async () => {
    const store = { hasSecret: jest.fn(async () => true) };
    const harness = await createHarness({ credentialStore: store });
    const result = await harness.callTool('SN-Register-Instance', {
      name: 'invalid-basic-grant',
      url: 'https://invalid.service-now.com',
      authType: 'basic',
      grantType: 'authorization_code',
      username: 'user'
    });
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload.code).toBe('INVALID_INSTANCE_CONFIG');
    expect(store.hasSecret).not.toHaveBeenCalled();
    expect(harness.registry.list()).toEqual([]);
  });

  test('rejects a non-boolean makeDefault when schema validation is bypassed', async () => {
    const harness = await createHarness();
    const result = await harness.callTool('SN-Register-Instance', {
      ...publicMetadata('invalid-default'),
      makeDefault: 'yes'
    });
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload.code).toBe('INVALID_INSTANCE_CONFIG');
    expect(harness.registry.list()).toEqual([]);
    expect(harness.configManager.reload).not.toHaveBeenCalled();
  });

  test('rejects duplicate names without replacing the existing metadata', async () => {
    const harness = await createHarness();
    const first = await harness.callTool('SN-Register-Instance', publicMetadata());
    const duplicate = await harness.callTool('SN-Register-Instance', {
      ...publicMetadata(),
      url: 'https://other.service-now.com',
      description: 'replacement'
    });
    const payload = parseResponse(duplicate);

    expect(parseResponse(first).success).toBe(true);
    expect(duplicate.isError).toBe(true);
    expect(payload.code).toBe('INSTANCE_ALREADY_EXISTS');
    expect(harness.registry.get('dev')).toEqual(expect.objectContaining({
      url: 'https://dev.service-now.com',
      clientId: 'public-client'
    }));
    expect(harness.configManager.reload).toHaveBeenCalledTimes(1);
  });

  test('does not mutate when credential backend fails and reports a safe error', async () => {
    const store = {
      hasSecret: jest.fn(async () => {
        throw new Error('keychain backend failed with fixture-secret');
      })
    };
    const harness = await createHarness({ credentialStore: store });
    const result = await harness.callTool('SN-Register-Instance', {
      name: 'broken-keychain',
      url: 'https://broken.service-now.com',
      authType: 'basic',
      username: 'user'
    });
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload.code).toBe('KEYCHAIN_UNAVAILABLE');
    expect(JSON.stringify(result)).not.toContain('fixture-secret');
    expect(harness.registry.list()).toEqual([]);
    expect(harness.configManager.reload).not.toHaveBeenCalled();
  });

  test('rolls back persisted metadata when config reload fails', async () => {
    const harness = await createHarness({ reload: false });
    const result = await harness.callTool('SN-Register-Instance', publicMetadata());
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({
      success: false,
      code: 'REGISTRY_RELOAD_FAILED',
      persisted: false,
      rolledBack: true
    });
    expect(() => harness.registry.get('dev')).toThrow(expect.objectContaining({ code: 'INSTANCE_NOT_FOUND' }));
    expect(JSON.stringify(result)).not.toContain('secret-value');
    expect(harness.configManager.reload).toHaveBeenCalledTimes(2);
  });
  test('requires a restart when restoring prior config also cannot complete', async () => {
    const harness = await createHarness();
    harness.configManager.reload.mockResolvedValue(false);
    const result = await harness.callTool('SN-Register-Instance', publicMetadata('restore-failed'));
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({
      success: false,
      code: 'REGISTRY_RELOAD_FAILED',
      persisted: false,
      rolledBack: true,
      restartRequired: true
    });
    expect(() => harness.registry.get('restore-failed')).toThrow(expect.objectContaining({ code: 'INSTANCE_NOT_FOUND' }));
  });


  test('reports rollback-required when compensation write fails', async () => {
    const harness = await createHarness();
    jest.spyOn(harness.registry, 'compensateRegistration').mockRejectedValue(
      Object.assign(new Error('rollback write failed'), { code: 'REGISTRY_WRITE_FAILED' })
    );
    harness.configManager.reload.mockRejectedValueOnce(new Error('reload failed'));

    const result = await harness.callTool('SN-Register-Instance', publicMetadata('rollback-write'));
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({
      success: false,
      code: 'REGISTRY_ROLLBACK_REQUIRED',
      persisted: true,
      partial: true,
      rollbackRequired: true,
      restartRequired: true
    });
  });

  test('logs only sanitized operation diagnostics for reload and rollback failures', async () => {
    const fixture = 'registration-diagnostic-secret-fixture';
    const harness = await createHarness();
    harness.configManager.reload.mockRejectedValueOnce(Object.assign(
      new Error(`reload failed ${fixture}`),
      { config: { credentialRef: `keychain:${fixture}` } }
    ));
    jest.spyOn(harness.registry, 'compensateRegistration').mockRejectedValueOnce(Object.assign(
      new Error(`rollback failed ${fixture}`),
      { code: 'REGISTRY_WRITE_FAILED', details: { credentialRef: `keychain:${fixture}` } }
    ));
    const diagnosticSpy = jest.spyOn(console, 'error').mockImplementation(() => {});

    const result = await harness.callTool('SN-Register-Instance', publicMetadata('diagnostic-failure'));
    const payload = parseResponse(result);
    const diagnostics = JSON.stringify(diagnosticSpy.mock.calls);

    expect(payload).toMatchObject({
      code: 'REGISTRY_ROLLBACK_REQUIRED',
      persisted: true,
      rollbackRequired: true
    });
    expect(diagnostics).toContain('registration_reload');
    expect(diagnostics).toContain('REGISTRY_RELOAD_FAILED');
    expect(diagnostics).toContain('registration_rollback');
    expect(diagnostics).toContain('REGISTRY_ROLLBACK_REQUIRED');
    expect(diagnostics).not.toContain(fixture);
    expect(diagnostics).not.toContain('credentialRef');
  });

  test('rolls back persisted metadata when the reload callback fails', async () => {
    const harness = await createHarness();
    const onConfigReload = jest.fn()
      .mockImplementationOnce(async () => {
        throw new Error('callback failed with secret-value');
      });
    const callbackServer = await createMcpServer({ setProgressCallback() {} }, {
      configManager: harness.configManager,
      instanceRegistry: harness.registry,
      credentialStore: harness.credentialStore,
      onConfigReload,
      createServiceNowClient: jest.fn()
    });
    const handler = callbackServer._requestHandlers.get('tools/call');
    const result = await handler({
      method: 'tools/call',
      params: { name: 'SN-Register-Instance', arguments: publicMetadata('callback') }
    }, {});
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({
      success: false,
      code: 'REGISTRY_RELOAD_FAILED',
      persisted: false,
      rolledBack: true
    });
    expect(() => harness.registry.get('callback')).toThrow(expect.objectContaining({ code: 'INSTANCE_NOT_FOUND' }));
    expect(JSON.stringify(result)).not.toContain('secret-value');
  });

  test('rejects registration when credentials disappear before the precommit write', async () => {
    const store = {
      hasSecret: jest.fn()
        .mockResolvedValueOnce(true)
        .mockResolvedValueOnce(false)
    };
    const harness = await createHarness({ credentialStore: store });
    const result = await harness.callTool('SN-Register-Instance', {
      name: 'dev',
      url: 'https://dev.service-now.com',
      authType: 'basic',
      username: 'developer'
    });
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({ success: false, code: 'CREDENTIAL_NOT_FOUND' });
    expect(harness.registry.list()).toEqual([]);
    expect(harness.configManager.reload).not.toHaveBeenCalled();
  });

  test('compensates when credentials disappear immediately after the registry commit', async () => {
    const store = {
      hasSecret: jest.fn()
        .mockResolvedValueOnce(true)
        .mockResolvedValueOnce(true)
        .mockResolvedValueOnce(false)
    };
    const harness = await createHarness({ credentialStore: store });
    const result = await harness.callTool('SN-Register-Instance', {
      name: 'dev',
      url: 'https://dev.service-now.com',
      authType: 'basic',
      username: 'developer'
    });
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({ success: false, code: 'CREDENTIAL_NOT_FOUND' });
    expect(harness.registry.list()).toEqual([]);
    expect(harness.configManager.reload).not.toHaveBeenCalled();
  });
  test('restores the old default when a makeDefault registration loses credentials after commit', async () => {
    const store = {
      hasSecret: jest.fn()
        .mockResolvedValueOnce(true)
        .mockResolvedValueOnce(true)
        .mockResolvedValueOnce(false)
    };
    const harness = await createHarness({ credentialStore: store });
    await harness.registry.register({
      name: 'old',
      url: 'https://old.service-now.com',
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'public-client'
    });

    const result = await harness.callTool('SN-Register-Instance', {
      name: 'new',
      url: 'https://new.service-now.com',
      authType: 'basic',
      username: 'developer',
      makeDefault: true
    });
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({ success: false, code: 'CREDENTIAL_NOT_FOUND' });
    expect(harness.registry.get('old')).toEqual(expect.objectContaining({ default: true }));
    expect(() => harness.registry.get('new')).toThrow(expect.objectContaining({ code: 'INSTANCE_NOT_FOUND' }));
  });
  test('keeps the first concurrent default when the second registration reload fails', async () => {
    const harness = await createHarness();
    harness.configManager.reload
      .mockImplementationOnce(() => Promise.resolve(harness.registry.reload()))
      .mockImplementationOnce(() => Promise.reject(new Error('second reload failed')));

    const [first, second] = await Promise.all([
      harness.callTool('SN-Register-Instance', publicMetadata('first')),
      harness.callTool('SN-Register-Instance', publicMetadata('second'))
    ]);

    expect(parseResponse(first)).toMatchObject({ success: true });
    expect(parseResponse(second)).toMatchObject({
      success: false,
      code: 'REGISTRY_RELOAD_FAILED',
      rolledBack: true
    });
    expect(harness.registry.get('first')).toEqual(expect.objectContaining({ default: true }));
    expect(() => harness.registry.get('second')).toThrow(expect.objectContaining({ code: 'INSTANCE_NOT_FOUND' }));
  });

  test('reports a partial rollback requirement when compensation detects a concurrent change', async () => {
    const store = {
      hasSecret: jest.fn()
        .mockResolvedValueOnce(true)
        .mockResolvedValueOnce(true)
        .mockResolvedValueOnce(false)
    };
    const harness = await createHarness({ credentialStore: store });
    jest.spyOn(harness.registry, 'compensateRegistration').mockRejectedValue(
      Object.assign(new Error('registration changed'), {
        code: 'REGISTRY_ROLLBACK_REQUIRED'
      })
    );

    const result = await harness.callTool('SN-Register-Instance', {
      name: 'new',
      url: 'https://new.service-now.com',
      authType: 'basic',
      username: 'developer'
    });
    const payload = parseResponse(result);

    expect(result.isError).toBe(true);
    expect(payload).toMatchObject({
      success: false,
      code: 'REGISTRY_ROLLBACK_REQUIRED',
      persisted: true,
      partial: true,
      rollbackRequired: true,
      restartRequired: true
    });
  });


  test('serializes concurrent duplicate registrations', async () => {
    const harness = await createHarness();
    const [first, second] = await Promise.all([
      harness.callTool('SN-Register-Instance', publicMetadata()),
      harness.callTool('SN-Register-Instance', publicMetadata())
    ]);
    const payloads = [parseResponse(first), parseResponse(second)];

    expect(payloads.filter(payload => payload.success)).toHaveLength(1);
    expect(payloads.filter(payload => payload.code === 'INSTANCE_ALREADY_EXISTS')).toHaveLength(1);
    expect(harness.registry.list()).toHaveLength(1);
    expect(harness.configManager.reload).toHaveBeenCalledTimes(1);
  });
});
