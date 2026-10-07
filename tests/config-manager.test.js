/**
 * Tests for ConfigManager.loadFromEnv() - OAuth client_credentials path
 *
 * Validates:
 * - SERVICENOW_OAUTH_GRANT_TYPE=client_credentials makes USERNAME/PASSWORD optional
 * - Grant type is propagated to the instance config so it reaches ServiceNowClient
 * - Original behaviour (basic auth, ROPC password grant) still requires USERNAME/PASSWORD
 */

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { InstanceRegistry } from '../src/instance-registry.js';
import { jest } from '@jest/globals';
import { ConfigManager, instanceToClientOptions } from '../src/config-manager.js';

describe('instanceToClientOptions()', () => {
  it('maps oauth authorization_code fields including the loopback config', () => {
    const opts = instanceToClientOptions({
      authType: 'oauth',
      clientId: 'cid',
      clientSecret: 'csec',
      grantType: 'authorization_code',
      scope: 'useraccount',
      authorizeUrl: 'https://x/oauth_auth.do',
      tokenUrl: 'https://x/oauth_token.do',
      redirectPort: 8455,
      callbackPath: '/callback'
    });
    expect(opts).toMatchObject({
      authType: 'oauth',
      clientId: 'cid',
      clientSecret: 'csec',
      grantType: 'authorization_code',
      scope: 'useraccount',
      authorizeUrl: 'https://x/oauth_auth.do',
      tokenUrl: 'https://x/oauth_token.do',
      redirectPort: 8455,
      callbackPath: '/callback'
    });
  });

  it('defaults authType to basic when the instance does not set it', () => {
    expect(instanceToClientOptions({}).authType).toBe('basic');
  });
  it('propagates credential references and injected credential stores while retaining legacy secrets', () => {
    const credentialStore = { getSecret: jest.fn() };
    const instance = {
      authType: 'oauth',
      clientId: 'cid',
      clientSecret: 'legacy-client-secret',
      password: 'legacy-password',
      credentialRef: {
        password: 'keychain:instance/dev/password',
        clientSecret: 'keychain:instance/dev/client-secret'
      }
    };

    expect(instanceToClientOptions(instance, { credentialStore })).toEqual(expect.objectContaining({
      clientSecret: 'legacy-client-secret',
      password: 'legacy-password',
      credentialRef: instance.credentialRef,
      credentialStore
    }));
  });
});

describe('ConfigManager.validateInstance()', () => {
  it('accepts a public authorization_code client without a secret or password', () => {
    const cm = new ConfigManager();

    expect(cm.validateInstance({
      name: 'public-oauth',
      url: 'https://example.service-now.com',
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'public-client-id'
    })).toBe(true);
  });
  it('uses registry credential validation for canonical refs', () => {
    const cm = new ConfigManager();

    expect(() => cm.validateInstance({
      name: 'bad-ref',
      url: 'https://bad-ref.service-now.com',
      authType: 'basic',
      username: 'user',
      credentialRef: 'plaintext-password'
    })).toThrow(expect.objectContaining({ code: 'INVALID_INSTANCE_CONFIG' }));
  });
});

describe('ConfigManager.loadFromEnv()', () => {
  const originalEnv = process.env;
  const basicAuthFixture = {
    user: 'unit-test-user',
    secret: 'unit-test-non-secret'
  };

  beforeEach(() => {
    process.env = { ...originalEnv };
    delete process.env.SERVICENOW_INSTANCE_URL;
    delete process.env.SERVICENOW_USERNAME;
    delete process.env.SERVICENOW_PASSWORD;
    delete process.env.SERVICENOW_AUTH_TYPE;
    delete process.env.SERVICENOW_CLIENT_ID;
    delete process.env.SERVICENOW_CLIENT_SECRET;
    delete process.env.SERVICENOW_OAUTH_GRANT_TYPE;
    delete process.env.SERVICENOW_OAUTH_SCOPE;
    delete process.env.SERVICENOW_OAUTH_AUTHORIZE_URL;
    delete process.env.SERVICENOW_OAUTH_TOKEN_URL;
    delete process.env.SERVICENOW_OAUTH_REDIRECT_PORT;
    delete process.env.SERVICENOW_OAUTH_CALLBACK_PATH;
  });

  afterAll(() => {
    process.env = originalEnv;
  });

  describe('basic auth (default)', () => {
    it('requires USERNAME and PASSWORD', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      const cm = new ConfigManager();
      expect(() => cm.loadFromEnv()).toThrow(/Missing ServiceNow credentials/);
    });

    it('loads instance with username and password set', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_USERNAME = basicAuthFixture.user;
      process.env.SERVICENOW_PASSWORD = basicAuthFixture.secret;
      const cm = new ConfigManager();
      const [instance] = cm.loadFromEnv();
      expect(instance.username).toBe(basicAuthFixture.user);
      expect(instance.password).toBe(basicAuthFixture.secret);
      expect(instance.authType).toBeUndefined();
      expect(instance.grantType).toBeUndefined();
    });
    it('delegates primary URL validation and canonicalization to the registry', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com/foo///';
      process.env.SERVICENOW_USERNAME = basicAuthFixture.user;
      process.env.SERVICENOW_PASSWORD = basicAuthFixture.secret;
      const cm = new ConfigManager();
      expect(cm.loadFromEnv()[0].url).toBe('https://example.service-now.com/foo');

      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com/foo?tenant=secret';
      expect(() => new ConfigManager().loadFromEnv()).toThrow(/query|fragment/i);
    });
  });

  describe('OAuth password grant (ROPC)', () => {
    it('still requires USERNAME and PASSWORD', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'password';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      process.env.SERVICENOW_CLIENT_SECRET = 'csec';
      const cm = new ConfigManager();
      expect(() => cm.loadFromEnv()).toThrow(/Missing ServiceNow credentials/);
    });

    it('propagates grantType to the instance config', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_USERNAME = basicAuthFixture.user;
      process.env.SERVICENOW_PASSWORD = basicAuthFixture.secret;
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'password';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      process.env.SERVICENOW_CLIENT_SECRET = 'csec';
      const cm = new ConfigManager();
      const [instance] = cm.loadFromEnv();
      expect(instance.authType).toBe('oauth');
      expect(instance.grantType).toBe('password');
      expect(instance.clientId).toBe('cid');
      expect(instance.clientSecret).toBe('csec');
    });
  });

  describe('OAuth client_credentials grant', () => {
    it('does NOT require USERNAME or PASSWORD', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'client_credentials';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      process.env.SERVICENOW_CLIENT_SECRET = 'csec';
      const cm = new ConfigManager();
      const [instance] = cm.loadFromEnv();
      expect(instance.url).toBe('https://example.service-now.com');
      expect(instance.username).toBe('');
      expect(instance.password).toBe('');
      expect(instance.authType).toBe('oauth');
      expect(instance.grantType).toBe('client_credentials');
      expect(instance.clientId).toBe('cid');
      expect(instance.clientSecret).toBe('csec');
    });

    it('still requires SERVICENOW_INSTANCE_URL', () => {
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'client_credentials';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      process.env.SERVICENOW_CLIENT_SECRET = 'csec';
      const cm = new ConfigManager();
      expect(() => cm.loadFromEnv()).toThrow(/Missing ServiceNow credentials/);
    });

    it('passes through SERVICENOW_OAUTH_SCOPE when set', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'client_credentials';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      process.env.SERVICENOW_CLIENT_SECRET = 'csec';
      process.env.SERVICENOW_OAUTH_SCOPE = 'useraccount';
      const cm = new ConfigManager();
      const [instance] = cm.loadFromEnv();
      expect(instance.scope).toBe('useraccount');
    });
  });

  it.each([
    ['missing OAuth clientId', { grantType: 'client_credentials', clientSecret: 'secret-to-protect' }],
    ['missing client_credentials clientSecret', { grantType: 'client_credentials', clientId: 'cid' }],
    ['missing password-grant username/password', { grantType: 'password', clientId: 'cid', clientSecret: 'secret-to-protect' }],
    ['missing password-grant clientSecret', {
      grantType: 'password',
      clientId: 'cid',
      username: 'oauth-user',
      password: 'oauth-password'
    }],
    ['missing authorization_code clientId', { grantType: 'authorization_code' }]
  ])('rejects %s with the stable startup error and no secret material', (_label, values) => {
    process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
    process.env.SERVICENOW_AUTH_TYPE = 'oauth';
    process.env.SERVICENOW_OAUTH_GRANT_TYPE = values.grantType;
    if (values.clientId) process.env.SERVICENOW_CLIENT_ID = values.clientId;
    if (values.clientSecret) process.env.SERVICENOW_CLIENT_SECRET = values.clientSecret;
    if (values.username) process.env.SERVICENOW_USERNAME = values.username;
    if (values.password) process.env.SERVICENOW_PASSWORD = values.password;

    const cm = new ConfigManager();
    let thrown;
    try {
      cm.loadFromEnv();
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(Error);
    expect(thrown.message).toMatch(/Missing ServiceNow credentials/);
    expect(thrown.message).not.toContain('secret-to-protect');
  });

  describe('OAuth authorization_code grant (per-user)', () => {
    it('does NOT require USERNAME or PASSWORD (browser sign-in supplies identity)', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'authorization_code';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      const cm = new ConfigManager();
      const [instance] = cm.loadFromEnv();
      expect(instance.authType).toBe('oauth');
      expect(instance.grantType).toBe('authorization_code');
      expect(instance.clientId).toBe('cid');
    });

    it('propagates the authorize/token endpoints and loopback config to the instance', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'authorization_code';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      process.env.SERVICENOW_OAUTH_AUTHORIZE_URL = 'https://example.service-now.com/oauth_auth.do';
      process.env.SERVICENOW_OAUTH_TOKEN_URL = 'https://example.service-now.com/oauth_token.do';
      process.env.SERVICENOW_OAUTH_REDIRECT_PORT = '8455';
      process.env.SERVICENOW_OAUTH_CALLBACK_PATH = '/callback';
      const cm = new ConfigManager();
      const [instance] = cm.loadFromEnv();
      expect(instance.authorizeUrl).toBe('https://example.service-now.com/oauth_auth.do');
      expect(instance.tokenUrl).toBe('https://example.service-now.com/oauth_token.do');
      expect(instance.redirectPort).toBe(8455);
      expect(instance.callbackPath).toBe('/callback');
    });

    it('rejects an external IdP endpoint from the environment unless the operator approved its origin', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'authorization_code';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      process.env.SERVICENOW_OAUTH_TOKEN_URL = 'https://login.example.com/token';
      delete process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS;
      expect(() => new ConfigManager().loadFromEnv()).toThrow(/SERVICENOW_OAUTH_TRUSTED_ORIGINS/);

      process.env.SERVICENOW_OAUTH_TRUSTED_ORIGINS = 'https://login.example.com';
      const [instance] = new ConfigManager().loadFromEnv();
      expect(instance.tokenUrl).toBe('https://login.example.com/token');
    });
    it.each(['12junk', '12.5', '1e2', '0x10', '+12', '-1', '   '])(
      'rejects non-decimal redirect port value %j',
      (value) => {
        process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
        process.env.SERVICENOW_AUTH_TYPE = 'oauth';
        process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'authorization_code';
        process.env.SERVICENOW_CLIENT_ID = 'cid';
        process.env.SERVICENOW_OAUTH_REDIRECT_PORT = value;
        const cm = new ConfigManager();
        expect(() => cm.loadFromEnv()).toThrow(/redirect port/i);
      }
    );

    it('trims a valid decimal redirect port before converting it', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'authorization_code';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      process.env.SERVICENOW_OAUTH_REDIRECT_PORT = ' 8455 ';
      const cm = new ConfigManager();
      expect(cm.loadFromEnv()[0].redirectPort).toBe(8455);
    });

    it.each(['65536', '999999999999999999999999999999999999'])(
      'rejects redirect port outside the valid range: %s',
      (value) => {
        process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
        process.env.SERVICENOW_AUTH_TYPE = 'oauth';
        process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'authorization_code';
        process.env.SERVICENOW_CLIENT_ID = 'cid';
        process.env.SERVICENOW_OAUTH_REDIRECT_PORT = value;
        const cm = new ConfigManager();
        expect(() => cm.loadFromEnv()).toThrow(/redirect port/i);
      }
    );

    it('throws a clear error when SERVICENOW_OAUTH_REDIRECT_PORT is not a number', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'authorization_code';
      process.env.SERVICENOW_CLIENT_ID = 'cid';
      process.env.SERVICENOW_OAUTH_REDIRECT_PORT = 'not-a-port';
      const cm = new ConfigManager();
      expect(() => cm.loadFromEnv()).toThrow(/redirect port/i);
    });
  });

  describe('missing-credentials error message', () => {
    it('mentions authorization_code as an exempt grant type', () => {
      process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
      process.env.SERVICENOW_AUTH_TYPE = 'oauth';
      process.env.SERVICENOW_OAUTH_GRANT_TYPE = 'password';
      const cm = new ConfigManager();
      expect(() => cm.loadFromEnv()).toThrow(/authorization_code/);
    });
  });
  it('keeps direct environment loads on the fallback facade', () => {
    process.env.SERVICENOW_INSTANCE_URL = 'https://example.service-now.com';
    process.env.SERVICENOW_USERNAME = basicAuthFixture.user;
    process.env.SERVICENOW_PASSWORD = basicAuthFixture.secret;
    const cm = new ConfigManager();

    const [loaded] = cm.loadFromEnv();

    expect(cm.getInstance('default')).toBe(loaded);
    expect(cm.getDefaultInstance()).toBe(loaded);
    expect(cm.listInstances()).toEqual([{
      name: 'default',
      url: 'https://example.service-now.com',
      default: true,
      description: 'Loaded from .env'
    }]);
  });
});

describe('ConfigManager registry facade', () => {
  test('delegates public reads and raw client reads to the injected registry', () => {
    const publicInstances = [{ name: 'dev', url: 'https://dev.service-now.com', default: true }];
    const rawInstances = [{
      ...publicInstances[0],
      password: 'legacy-password-fixture',
      clientSecret: 'legacy-client-secret-fixture'
    }];
    const registry = {
      load: jest.fn(() => ({ version: 1, instances: publicInstances })),
      reload: jest.fn(() => ({ version: 1, instances: publicInstances })),
      get: jest.fn(() => publicInstances[0]),
      getDefault: jest.fn(() => publicInstances[0]),
      list: jest.fn(() => publicInstances),
      _getForClient: jest.fn(() => rawInstances[0]),
      _getDefaultForClient: jest.fn(() => rawInstances[0]),
      _listForClient: jest.fn(() => rawInstances),
      validate: jest.fn(() => true),
      hasFile: jest.fn(() => true)
    };
    const cm = new ConfigManager({ registry });

    expect(cm.loadInstances()).toBe(rawInstances);
    expect(cm.getInstance('dev')).toBe(rawInstances[0]);
    expect(cm.getDefaultInstance()).toBe(rawInstances[0]);
    const listedInstances = cm.listInstances();
    expect(listedInstances).toEqual([{
      name: 'dev',
      url: 'https://dev.service-now.com',
      default: true,
      description: ''
    }]);
    expect(JSON.stringify(listedInstances)).not.toContain('legacy-password-fixture');
    expect(JSON.stringify(listedInstances)).not.toContain('legacy-client-secret-fixture');
    expect(cm.validateInstance(publicInstances[0])).toBe(true);
    expect(cm.reload()).toBe(rawInstances);
    expect(registry.load).toHaveBeenCalledTimes(1);
    expect(registry.reload).toHaveBeenCalledTimes(1);
    expect(registry._getForClient).toHaveBeenCalledWith('dev');
    expect(registry._getDefaultForClient).toHaveBeenCalledTimes(1);
    expect(registry.list).toHaveBeenCalledTimes(1);
    expect(registry._listForClient).toHaveBeenCalledTimes(2);
    expect(registry.validate).toHaveBeenCalledWith(publicInstances[0]);
  });
  test('strips unsupported secret-shaped fields from raw client reads', () => {
    const rawInstance = {
      name: 'unsafe',
      url: 'https://unsafe.service-now.com',
      username: 'user',
      password: 'legacy-password',
      clientSecret: 'legacy-client-secret',
      accessToken: 'access-token',
      refreshToken: 'refresh-token',
      apiKey: 'api-key',
      githubToken: 'github-token',
      privateKey: 'private-key',
      tokenUrl: 'https://unsafe.service-now.com/oauth/token'
    };
    const registry = {
      load: jest.fn(() => ({ version: 1, instances: [rawInstance] })),
      hasFile: jest.fn(() => true),
      _listForClient: jest.fn(() => [rawInstance]),
      _getForClient: jest.fn(() => rawInstance),
      _getDefaultForClient: jest.fn(() => rawInstance)
    };
    const cm = new ConfigManager({ registry });

    expect(cm.loadInstances()[0]).toEqual(expect.objectContaining({
      password: 'legacy-password',
      clientSecret: 'legacy-client-secret',
      tokenUrl: rawInstance.tokenUrl
    }));
    for (const key of ['accessToken', 'refreshToken', 'apiKey', 'githubToken', 'privateKey']) {
      expect(cm.loadInstances()[0]).not.toHaveProperty(key);
    }
    expect(cm.getInstance('unsafe')).not.toHaveProperty('accessToken');
    expect(cm.getDefaultInstance()).not.toHaveProperty('githubToken');
  });

  test('keeps legacy credentials available only through ConfigManager client reads', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-config-manager-legacy-'));
    const file = path.join(dir, 'instances.json');
    fs.writeFileSync(file, JSON.stringify({
      version: 1,
      instances: [{
        name: 'legacy',
        url: 'https://legacy.service-now.com',
        username: 'legacy-user',
        password: 'legacy-password-fixture',
        clientSecret: 'legacy-client-secret-fixture',
        default: true
      }]
    }));

    try {
      const registry = new InstanceRegistry({ readPath: file, writePath: file });
      const cm = new ConfigManager({ registry });

      expect(cm.getInstance('legacy')).toEqual(expect.objectContaining({
        password: 'legacy-password-fixture',
        clientSecret: 'legacy-client-secret-fixture'
      }));
      expect(JSON.stringify(cm.listInstances())).not.toContain('legacy-password-fixture');
      expect(JSON.stringify(cm.listInstances())).not.toContain('legacy-client-secret-fixture');
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  test('falls back to environment credentials only when the registry file is absent', () => {
    const registry = {
      load: jest.fn(() => ({ version: 1, instances: [] })),
      hasFile: jest.fn(() => false)
    };
    process.env.SERVICENOW_INSTANCE_URL = 'https://env.service-now.com';
    process.env.SERVICENOW_USERNAME = 'env-user';
    process.env.SERVICENOW_PASSWORD = 'env-password-fixture';
    const cm = new ConfigManager({ registry });

    expect(cm.loadInstances()).toEqual(expect.arrayContaining([
      expect.objectContaining({ name: 'default', url: 'https://env.service-now.com' })
    ]));
    expect(registry.load).toHaveBeenCalledTimes(1);
  });

  test('uses the configured registry instead of environment fallback when a file exists', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-config-manager-'));
    const file = path.join(dir, 'instances.json');
    fs.writeFileSync(file, JSON.stringify({
      version: 1,
      instances: [{
        name: 'configured',
        url: 'https://configured.service-now.com',
        authType: 'oauth',
        grantType: 'authorization_code',
        clientId: 'configured-client',
        default: true
      }]
    }));
    process.env.SERVICENOW_INSTANCE_URL = 'https://env.service-now.com';
    process.env.SERVICENOW_USERNAME = 'env-user';
    process.env.SERVICENOW_PASSWORD = 'env-password-fixture';
    const cm = new ConfigManager({
      registry: new InstanceRegistry({ readPath: file, writePath: file })
    });

    expect(cm.getDefaultInstance().name).toBe('configured');
    fs.rmSync(dir, { recursive: true, force: true });
  });
  test('resolves HAPPY_CONFIG_PATH after ConfigManager construction', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-config-manager-env-path-'));
    const file = path.join(dir, 'instances.json');
    const originalConfigPath = process.env.HAPPY_CONFIG_PATH;

    try {
      delete process.env.HAPPY_CONFIG_PATH;
      const cm = new ConfigManager();
      fs.writeFileSync(file, JSON.stringify({
        version: 1,
        instances: [{
          name: 'configured',
          url: 'https://configured.service-now.com',
          username: 'configured-user',
          password: 'configured-password-fixture',
          clientSecret: 'configured-client-secret-fixture',
          default: true
        }]
      }));
      process.env.HAPPY_CONFIG_PATH = file;

      const loaded = cm.getDefaultInstance();
      expect(loaded.name).toBe('configured');
      expect(loaded.password).toBe('configured-password-fixture');
      expect(loaded.clientSecret).toBe('configured-client-secret-fixture');
      const listed = JSON.stringify(cm.listInstances());
      expect(listed).not.toContain('configured-password-fixture');
      expect(listed).not.toContain('configured-client-secret-fixture');

      fs.writeFileSync(file, JSON.stringify({
        version: 1,
        instances: [{
          name: 'reloaded',
          url: 'https://reloaded.service-now.com',
          username: 'reloaded-user',
          password: 'reloaded-password-fixture',
          default: true
        }]
      }));
      expect(cm.reload()[0].name).toBe('reloaded');
    } finally {
      if (originalConfigPath === undefined) {
        delete process.env.HAPPY_CONFIG_PATH;
      } else {
        process.env.HAPPY_CONFIG_PATH = originalConfigPath;
      }
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
  test('restores the environment fallback cache when a malformed registry appears', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-config-manager-reload-'));
    const file = path.join(dir, 'instances.json');
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    process.env.SERVICENOW_INSTANCE_URL = 'https://env.service-now.com';
    process.env.SERVICENOW_USERNAME = 'env-user';
    process.env.SERVICENOW_PASSWORD = 'env-password-fixture';
    const cm = new ConfigManager({ registry });

    try {
      const previousInstances = cm.loadInstances();
      fs.writeFileSync(file, '{"version":1,"instances":[}\n');

      expect(() => cm.reload()).toThrow(expect.objectContaining({
        code: 'REGISTRY_RELOAD_FAILED'
      }));
      expect(cm.loadInstances()).toBe(previousInstances);
      expect(cm.getInstance('default')).toBe(previousInstances[0]);
      expect(cm.getDefaultInstance()).toBe(previousInstances[0]);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  test('throws a typed REGISTRY_EMPTY error after removing the last persisted instance', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-config-manager-empty-'));
    const file = path.join(dir, 'instances.json');
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const cm = new ConfigManager({ registry });
    const previousInstance = process.env.SERVICENOW_INSTANCE;

    try {
      delete process.env.SERVICENOW_INSTANCE;
      await registry.register({
        name: 'only',
        url: 'https://only.service-now.com',
        authType: 'oauth',
        grantType: 'authorization_code',
        clientId: 'only-client',
        default: true
      });
      await registry.remove('only');

      expect(() => cm.getDefaultInstance()).toThrow(expect.objectContaining({
        code: 'REGISTRY_EMPTY'
      }));
      expect(() => cm.getInstanceOrDefault()).toThrow(expect.objectContaining({
        code: 'REGISTRY_EMPTY'
      }));
    } finally {
      if (previousInstance === undefined) {
        delete process.env.SERVICENOW_INSTANCE;
      } else {
        process.env.SERVICENOW_INSTANCE = previousInstance;
      }
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
});
