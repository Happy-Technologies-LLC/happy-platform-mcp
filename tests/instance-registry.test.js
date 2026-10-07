import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, beforeEach, describe, expect, jest, test } from '@jest/globals';
import { InstanceRegistry, InstanceRegistryError } from '../src/instance-registry.js';
import { credentialRefFor } from '../src/instance-credential-store.js';

const tempDirs = [];

function tempPaths() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-instance-registry-'));
  tempDirs.push(dir);
  return {
    dir,
    file: path.join(dir, 'instances.json'),
    legacyFile: path.join(dir, 'legacy.json'),
    userFile: path.join(dir, 'user', 'instances.json')
  };
}

function writeJson(file, document) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(document)}\n`);
}

function publicInstance(name, extra = {}) {
  return {
    name,
    url: `https://${name}.service-now.com`,
    authType: 'oauth',
    grantType: 'authorization_code',
    clientId: `${name}-public-client`,
    ...extra
  };
}

afterEach(() => {
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
  jest.restoreAllMocks();
});

describe('InstanceRegistry reads', () => {
  test('loads versioned instances and preserves unrelated document properties', () => {
    const { file } = tempPaths();
    writeJson(file, {
      version: 1,
      docs: { localIndexEnabled: true },
      other: { preserved: 'yes' },
      instances: [publicInstance('dev', { default: true })]
    });

    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    expect(registry.load()).toEqual(expect.objectContaining({ version: 1 }));
    expect(registry.list()).toHaveLength(1);
    expect(registry.get('dev').clientId).toBe('dev-public-client');
    expect(registry.document.docs.localIndexEnabled).toBe(true);
    expect(registry.document.other.preserved).toBe('yes');
  });

  test('returns an empty versioned document when the registry does not exist', () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    expect(registry.load()).toEqual({ version: 1, instances: [] });
    expect(registry.list()).toEqual([]);
    expect(registry.getDefault()).toBeUndefined();
  });
  test('rejects multiple default instances in a persisted document', () => {
    const { file } = tempPaths();
    writeJson(file, {
      version: 1,
      instances: [
        publicInstance('dev', { default: true }),
        publicInstance('prod', { default: true })
      ]
    });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    expect(() => registry.load()).toThrow(
      expect.objectContaining({ code: 'REGISTRY_RELOAD_FAILED' })
    );
  });
});

describe('InstanceRegistry validation and defaults', () => {
  test('rejects duplicate names and invalid non-HTTPS URLs', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await registry.register(publicInstance('dev'));

    await expect(registry.register(publicInstance('dev', { clientId: 'other' })))
      .rejects.toMatchObject({ code: 'INSTANCE_ALREADY_EXISTS' });
    await expect(registry.register({
      name: 'bad',
      url: 'http://remote.service-now.com',
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'public-client'
    })).rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
  });

  test('allows explicit loopback HTTP URLs only', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    await expect(registry.register(publicInstance('loopback', {
      url: 'http://127.0.0.1:8080'
    }))).resolves.toEqual(expect.objectContaining({ name: 'loopback' }));
    await expect(registry.register(publicInstance('remote', {
      url: 'http://localhost.example.com'
    }))).rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
  });

  test('makes the first registration default and makeDefault changes it', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    await registry.register(publicInstance('dev'));
    await registry.register(publicInstance('prod'));
    expect(registry.getDefault().name).toBe('dev');

    await registry.update('prod', { description: 'Production', default: true });
    expect(registry.getDefault().name).toBe('prod');
    expect(registry.list().filter(instance => instance.default)).toHaveLength(1);

    await registry.register(publicInstance('staging'), { makeDefault: true });
    expect(registry.getDefault().name).toBe('staging');
    expect(registry.list().filter(instance => instance.default)).toHaveLength(1);
  });

  test('enforces auth requirements for basic, client credentials, and password grants', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    await expect(registry.register({
      name: 'basic', url: 'https://basic.service-now.com', authType: 'basic', username: 'user'
    })).rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
    await expect(registry.register({
      name: 'cc', url: 'https://cc.service-now.com', authType: 'oauth', grantType: 'client_credentials', credentialRef: 'secret/ref'
    })).rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
    await expect(registry.register({
      name: 'ropc', url: 'https://ropc.service-now.com', authType: 'oauth', grantType: 'password', username: 'user', clientId: 'cid', credentialRef: { password: 'password/ref' }
    })).rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });

    await expect(registry.register({
      name: 'basic-ok', url: 'https://basic-ok.service-now.com', authType: 'basic', username: 'user', credentialRef: credentialRefFor('basic-ok', 'password')
    })).resolves.toEqual(expect.objectContaining({ name: 'basic-ok' }));
    await expect(registry.register({
      name: 'cc-ok', url: 'https://cc-ok.service-now.com', authType: 'oauth', grantType: 'client_credentials', clientId: 'cid', credentialRef: credentialRefFor('cc-ok', 'client-secret')
    })).resolves.toEqual(expect.objectContaining({ name: 'cc-ok' }));
    await expect(registry.register({
      name: 'ropc-ok', url: 'https://ropc-ok.service-now.com', authType: 'oauth', grantType: 'password', username: 'user', clientId: 'cid', credentialRef: {
        password: credentialRefFor('ropc-ok', 'password'),
        clientSecret: credentialRefFor('ropc-ok', 'client-secret')
      }
    })).resolves.toEqual(expect.objectContaining({ name: 'ropc-ok' }));
  });

  test('requires refs to be canonical, instance-bound, and type-bound before persistence', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const cases = [
      {
        name: 'basic-arbitrary',
        url: 'https://basic-arbitrary.service-now.com',
        authType: 'basic',
        username: 'user',
        credentialRef: 'plaintext-password'
      },
      {
        name: 'basic-cross-instance',
        url: 'https://basic-cross-instance.service-now.com',
        authType: 'basic',
        username: 'user',
        credentialRef: credentialRefFor('other', 'password')
      },
      {
        name: 'basic-wrong-type',
        url: 'https://basic-wrong-type.service-now.com',
        authType: 'basic',
        username: 'user',
        credentialRef: credentialRefFor('basic-wrong-type', 'client-secret')
      },
      {
        name: 'oauth-cross-instance',
        url: 'https://oauth-cross-instance.service-now.com',
        authType: 'oauth',
        grantType: 'client_credentials',
        clientId: 'cid',
        credentialRef: credentialRefFor('other', 'client-secret')
      },
      {
        name: 'oauth-percent-variant',
        url: 'https://oauth-percent-variant.service-now.com',
        authType: 'oauth',
        grantType: 'client_credentials',
        clientId: 'cid',
        credentialRef: 'keychain:instance/oauth-percent%2dvariant/client-secret'
      },
      {
        name: 'oauth-traversal-variant',
        url: 'https://oauth-traversal-variant.service-now.com',
        authType: 'oauth',
        grantType: 'client_credentials',
        clientId: 'cid',
        credentialRef: 'keychain:instance/../client-secret'
      },
      {
        name: 'oauth-case-variant',
        url: 'https://oauth-case-variant.service-now.com',
        authType: 'oauth',
        grantType: 'client_credentials',
        clientId: 'cid',
        credentialRef: 'keychain:instance/OAuth-case-variant/client-secret'
      },
      {
        name: 'oauth-password-invalid-shape',
        url: 'https://oauth-password-invalid-shape.service-now.com',
        authType: 'oauth',
        grantType: 'password',
        username: 'user',
        clientId: 'cid',
        credentialRef: credentialRefFor('oauth-password-invalid-shape', 'client-secret')
      },
      {
        name: 'oauth-password-cross-instance',
        url: 'https://oauth-password-cross-instance.service-now.com',
        authType: 'oauth',
        grantType: 'password',
        username: 'user',
        clientId: 'cid',
        credentialRef: {
          password: credentialRefFor('other', 'password'),
          clientSecret: credentialRefFor('oauth-password-cross-instance', 'client-secret')
        }
      }
    ];

    for (const instance of cases) {
      await expect(registry.register(instance))
        .rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
      expect(registry.list()).not.toEqual(expect.arrayContaining([
        expect.objectContaining({ name: instance.name })
      ]));
    }
  });

  test('keeps canonical refs visible in redacted public metadata', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const ref = credentialRefFor('visible-ref', 'password');
    await registry.register({
      name: 'visible-ref',
      url: 'https://visible-ref.service-now.com',
      authType: 'basic',
      username: 'user',
      credentialRef: ref
    });
    expect(registry.get('visible-ref').credentialRef).toBe(ref);
  });
  test('infers OAuth grant type from username presence when omitted', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    await expect(registry.register({
      name: 'client-credentials-inferred',
      url: 'https://client-credentials-inferred.service-now.com',
      authType: 'oauth',
      clientId: 'cid',
      credentialRef: credentialRefFor('client-credentials-inferred', 'client-secret')
    })).resolves.toEqual(expect.objectContaining({
      name: 'client-credentials-inferred',
      authType: 'oauth'
    }));

    await expect(registry.register({
      name: 'password-inferred',
      url: 'https://password-inferred.service-now.com',
      authType: 'oauth',
      username: 'user',
      clientId: 'cid',
      credentialRef: {
        password: credentialRefFor('password-inferred', 'password'),
        clientSecret: credentialRefFor('password-inferred', 'client-secret')
      }
    })).resolves.toEqual(expect.objectContaining({
      name: 'password-inferred',
      authType: 'oauth'
    }));

    expect(registry.get('client-credentials-inferred').grantType).toBeUndefined();
    expect(registry.get('password-inferred').grantType).toBeUndefined();
    expect(registry.validate({
      name: 'client-credentials-validation',
      url: 'https://client-credentials-validation.service-now.com',
      authType: 'oauth',
      clientId: 'cid',
      credentialRef: credentialRefFor('client-credentials-validation', 'client-secret')
    })).toBe(true);
  });

  test('allows a public authorization-code client without a credential reference', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    await expect(registry.register(publicInstance('public'))).resolves.toEqual(expect.objectContaining({ name: 'public' }));
  });

  test('rejects unknown fields and secret values without echoing the submitted object', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    for (const field of ['password', 'clientSecret', 'unexpected']) {
      await expect(registry.register({
        ...publicInstance(`bad-${field}`),
        [field]: 'fixture-secret-value'
      })).rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
      await expect(registry.register({
        ...publicInstance(`bad-${field}-again`),
        [field]: 'fixture-secret-value'
      })).rejects.toThrow(/^(?!.*fixture-secret-value).*$/s);
    }
  });
  test('rejects unsupported legacy instance secret fields even when supported plaintext credentials exist', () => {
    const { file } = tempPaths();
    writeJson(file, {
      version: 1,
      instances: [{
        name: 'legacy',
        url: 'https://legacy.service-now.com',
        authType: 'basic',
        username: 'user',
        password: 'legacy-password',
        accessToken: 'unsupported-token'
      }]
    });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    expect(() => registry.load()).toThrow(expect.objectContaining({ code: 'REGISTRY_RELOAD_FAILED' }));
  });

  test('does not let docs.githubToken enable instance secret fields', () => {
    const { file } = tempPaths();
    writeJson(file, {
      version: 1,
      docs: { githubToken: 'docs-token' },
      instances: [publicInstance('public')]
    });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    expect(registry.load().docs).not.toHaveProperty('githubToken');
    expect(registry.get('public')).not.toHaveProperty('githubToken');
  });

  test('rejects invalid provided schema field types and malformed endpoints', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const cases = [
      ['authType', null, publicInstance('bad-auth-null')],
      ['authType', '', publicInstance('bad-auth-empty')],
      ['authType', false, publicInstance('bad-auth-false')],
      ['grantType', null, publicInstance('bad-grant-null')],
      ['grantType', '', publicInstance('bad-grant-empty')],
      ['grantType', false, publicInstance('bad-grant-false')],
      ['username', 42, { name: 'bad-username', url: 'https://bad-username.service-now.com', authType: 'basic', credentialRef: 'ref' }],
      ['clientId', 42, publicInstance('bad-client-id')],
      ['credentialRef', 42, { name: 'bad-credential-ref', url: 'https://bad-credential-ref.service-now.com', authType: 'basic', username: 'user' }],
      ['scope', 42, publicInstance('bad-scope')],
      ['authorizeUrl', 'not a url', publicInstance('bad-authorize-url')],
      ['tokenUrl', 42, publicInstance('bad-token-url')],
      ['callbackPath', 'callback', publicInstance('bad-callback-path')],
      ['callbackPath', 42, publicInstance('bad-callback-type')],
      ['description', 42, publicInstance('bad-description')]
    ];

    for (const [field, value, instance] of cases) {
      await expect(registry.register({ ...instance, [field]: value }))
        .rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
    }
  });

  test('validates optional username and clientId in every auth branch', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const cases = [
      {
        name: 'basic-invalid-client-id',
        url: 'https://basic-invalid-client-id.service-now.com',
        authType: 'basic',
        username: 'user',
        clientId: false,
        credentialRef: 'password/ref'
      },
      {
        name: 'oauth-invalid-username',
        url: 'https://oauth-invalid-username.service-now.com',
        authType: 'oauth',
        grantType: 'authorization_code',
        username: 0,
        clientId: 'cid'
      },
      {
        name: 'client-credentials-invalid-username',
        url: 'https://client-credentials-invalid-username.service-now.com',
        authType: 'oauth',
        grantType: 'client_credentials',
        username: '',
        clientId: 'cid',
        credentialRef: 'secret/ref'
      }
    ];

    for (const instance of cases) {
      await expect(registry.register(instance))
        .rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
    }
  });

  test('rejects URL userinfo in primary and optional endpoint URLs', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const cases = [
      publicInstance('userinfo-primary', {
        url: 'https://user:password@userinfo-primary.service-now.com'
      }),
      publicInstance('userinfo-authorize', {
        authorizeUrl: 'https://user:password@oauth.service-now.com/oauth_auth.do'
      }),
      publicInstance('userinfo-token', {
        tokenUrl: 'https://user:password@oauth.service-now.com/oauth_token.do'
      })
    ];

    for (const instance of cases) {
      await expect(registry.register(instance))
        .rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
    }
  });
  test('rejects search and fragment components on the primary instance URL', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    for (const url of [
      'https://query.service-now.com/instance?tenant=secret',
      'https://fragment.service-now.com/instance#section'
    ]) {
      await expect(registry.register(publicInstance(`reject-${url.includes('?') ? 'query' : 'hash'}`, { url })))
        .rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
    }
  });

  test('canonicalizes primary URL trailing slashes without dropping path prefixes', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await registry.register(publicInstance('path-prefix', {
      url: 'https://path-prefix.service-now.com/foo/bar///'
    }));
    expect(registry.get('path-prefix').url).toBe('https://path-prefix.service-now.com/foo/bar');
  });

  test('allows authorize and token URL queries while rejecting primary URL queries', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await expect(registry.register(publicInstance('oauth-query', {
      authorizeUrl: 'https://oauth-query.service-now.com/authorize?client=cid',
      tokenUrl: 'https://oauth-query.service-now.com/token?audience=sn'
    }))).resolves.toEqual(expect.objectContaining({ name: 'oauth-query' }));
  });

  test('validates legacy plaintext password and clientSecret values when present', () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const cases = [
      {
        name: 'legacy-basic-password',
        url: 'https://legacy-basic-password.service-now.com',
        authType: 'basic',
        username: 'user',
        password: false,
        credentialRef: 'password/ref'
      },
      {
        name: 'legacy-client-secret',
        url: 'https://legacy-client-secret.service-now.com',
        authType: 'oauth',
        grantType: 'client_credentials',
        clientId: 'cid',
        clientSecret: 0,
        credentialRef: 'secret/ref'
      }
    ];

    for (const instance of cases) {
      expect(() => registry.validate(instance))
        .toThrow(expect.objectContaining({ code: 'INVALID_INSTANCE_CONFIG' }));
    }
  });
  test('reload rejects invalid credential references even when legacy plaintext is present', () => {
    const { file } = tempPaths();
    writeJson(file, {
      version: 1,
      instances: [{
        name: 'legacy-ref',
        url: 'https://legacy-ref.service-now.com',
        authType: 'basic',
        username: 'user',
        password: 'legacy-password',
        credentialRef: 'plaintext-password'
      }]
    });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    expect(() => registry.load()).toThrow(expect.objectContaining({ code: 'REGISTRY_RELOAD_FAILED' }));
  });

  test('public redaction omits invalid credential references from injected documents', () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    registry._document = {
      version: 1,
      instances: [{
        name: 'injected',
        url: 'https://injected.service-now.com',
        authType: 'basic',
        username: 'user',
        credentialRef: 'plaintext-password'
      }]
    };
    registry._loaded = true;
    expect(registry.list()[0]).not.toHaveProperty('credentialRef');
  });
  test('rejects malformed legacy instances with a stable reload error', () => {
    const { file } = tempPaths();
    const cases = [
      {
        name: 'missing URL',
        instances: [{
          name: 'missing-url',
          authType: 'basic',
          username: 'user',
          password: 'secret'
        }]
      },
      {
        name: 'wrong secret type',
        instances: [{
          name: 'wrong-secret-type',
          url: 'https://wrong-secret-type.service-now.com',
          authType: 'basic',
          username: 'user',
          password: 42
        }]
      },
      {
        name: 'wrong structural type',
        instances: [{
          name: 'wrong-structural-type',
          url: 'https://wrong-structural-type.service-now.com',
          authType: 'oauth',
          grantType: 'client_credentials',
          clientId: 42,
          clientSecret: 'secret'
        }]
      },
      {
        name: 'duplicate names',
        instances: [
          {
            name: 'duplicate',
            url: 'https://duplicate.service-now.com',
            authType: 'basic',
            username: 'user',
            password: 'secret'
          },
          {
            name: 'duplicate',
            url: 'https://duplicate.service-now.com',
            authType: 'basic',
            username: 'user',
            password: 'secret'
          }
        ]
      },
      {
        name: 'multiple defaults',
        instances: [
          {
            name: 'default-one',
            url: 'https://default-one.service-now.com',
            authType: 'basic',
            username: 'user',
            password: 'secret',
            default: true
          },
          {
            name: 'default-two',
            url: 'https://default-two.service-now.com',
            authType: 'basic',
            username: 'user',
            password: 'secret',
            default: true
          }
        ]
      },
      {
        name: 'unsafe HTTP',
        instances: [{
          name: 'unsafe-http',
          url: 'http://unsafe-http.service-now.com',
          authType: 'basic',
          username: 'user',
          password: 'secret'
        }]
      }
    ];

    for (const document of cases) {
      writeJson(file, document);
      const registry = new InstanceRegistry({ readPath: file, writePath: file });
      expect(() => registry.load()).toThrow(expect.objectContaining({
        code: 'REGISTRY_RELOAD_FAILED'
      }));
    }
  });
});

describe('InstanceRegistry persistence', () => {
  test('recursively redacts legacy plaintext credentials from public views', async () => {
    const { file } = tempPaths();
    writeJson(file, {
      metadata: {
        password: 'nested-document-password-fixture',
        clientSecret: 'nested-document-client-secret-fixture'
      },
      instances: [{
        name: 'legacy',
        url: 'https://legacy.service-now.com',
        authType: 'oauth',
        grantType: 'password',
        clientId: 'legacy-client',
        username: 'legacy-user',
        password: 'legacy-password-fixture',
        clientSecret: 'legacy-client-secret-fixture',
        credentialRef: {
          password: credentialRefFor('legacy', 'password'),
          clientSecret: credentialRefFor('legacy', 'client-secret')
        },
        default: true
      }]
    });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    for (const view of [
      registry.load(),
      registry.document,
      registry.list()[0],
      registry.get('legacy'),
      registry.getDefault()
    ]) {
      const serialized = JSON.stringify(view);
      expect(serialized).not.toContain('legacy-password-fixture');
      expect(serialized).not.toContain('legacy-client-secret-fixture');
      expect(serialized).not.toContain('nested-password-fixture');
      expect(serialized).not.toContain('nested-client-secret-fixture');
      expect(serialized).not.toContain('nested-document-password-fixture');
      expect(serialized).not.toContain('nested-document-client-secret-fixture');
    }
    expect(registry.load().instances[0].credentialRef).toEqual({
      password: credentialRefFor('legacy', 'password'),
      clientSecret: credentialRefFor('legacy', 'client-secret')
    });
    expect(registry.listForClient).toBeUndefined();
    expect(registry.getForClient).toBeUndefined();
    expect(registry.getDefaultForClient).toBeUndefined();

    await expect(registry.update('legacy', { description: 'still legacy' }))
      .rejects.toMatchObject({ code: 'LEGACY_MIGRATION_REQUIRED' });
    await expect(registry.remove('legacy'))
      .rejects.toMatchObject({ code: 'LEGACY_MIGRATION_REQUIRED' });
  });

  test('redacts common secret keys recursively without stripping tokenUrl', async () => {
    const { file } = tempPaths();
    const secrets = {
      password: 'password-fixture',
      clientSecret: 'client-secret-fixture',
      githubToken: 'github-token-fixture',
      accessToken: 'access-token-fixture',
      refreshToken: 'refresh-token-fixture',
      apiKey: 'api-key-fixture',
      token: 'token-fixture'
    };
    writeJson(file, {
      docs: {
        githubToken: secrets.githubToken,
        tokenUrl: 'https://docs.example.com/token'
      },
      metadata: {
        nested: {
          ...secrets,
          tokenUrl: 'https://metadata.example.com/token'
        }
      },
      instances: [{
        ...publicInstance('tokenized', {
          grantType: 'client_credentials',
          tokenUrl: 'https://tokenized.service-now.com/oauth_token.do',
          credentialRef: credentialRefFor('tokenized', 'client-secret')
        })
      }]
    });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    const publicView = registry.load();
    expect(publicView.docs.tokenUrl).toBe('https://docs.example.com/token');
    expect(publicView.metadata.nested.tokenUrl).toBe('https://metadata.example.com/token');
    expect(publicView.instances[0].tokenUrl).toBe('https://tokenized.service-now.com/oauth_token.do');
    expect(publicView.instances[0].credentialRef).toBe(credentialRefFor('tokenized', 'client-secret'));
    await expect(registry.register(publicInstance('new')))
      .resolves.toEqual(expect.objectContaining({ name: 'new' }));
  });
  test('redacts every unknown secret-shaped key while preserving safe token URLs and canonical refs', () => {
    const { file } = tempPaths();
    const ref = credentialRefFor('nested-redaction', 'client-secret');
    writeJson(file, {
      docs: {
        privateKey: 'private-key-fixture',
        private_key: 'private-key-fixture',
        'private-key': 'private-key-fixture',
        client_secret: 'client-secret-fixture',
        'client-secret': 'client-secret-fixture',
        access_token: 'access-fixture',
        refreshToken: 'refresh-fixture',
        api_keys: ['api-fixture'],
        tokenUrl: 'https://safe.example/token'
      },
      instances: [publicInstance('nested-redaction', {
        grantType: 'client_credentials',
        credentialRef: ref,
        tokenUrl: 'https://nested-redaction.service-now.com/token'
      })]
    });
    const publicView = new InstanceRegistry({ readPath: file, writePath: file }).load();
    const serialized = JSON.stringify(publicView);
    expect(serialized).not.toContain('fixture');
    expect(publicView.docs.tokenUrl).toBe('https://safe.example/token');
    expect(publicView.instances[0].tokenUrl).toBe('https://nested-redaction.service-now.com/token');
    expect(publicView.instances[0].credentialRef).toBe(ref);
  });

  test('normalizes equal credential aliases and rejects conflicting aliases before persistence', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const password = credentialRefFor('alias-equal', 'password');
    const clientSecret = credentialRefFor('alias-equal', 'client-secret');
    await registry.register({
      name: 'alias-equal',
      url: 'https://alias-equal.service-now.com',
      authType: 'oauth',
      grantType: 'password',
      username: 'user',
      clientId: 'cid',
      credentialRef: {
        password,
        passwordRef: password,
        clientSecret,
        clientSecretRef: clientSecret
      }
    });
    const persisted = JSON.parse(fs.readFileSync(file, 'utf8')).instances[0].credentialRef;
    expect(persisted).toEqual({ password, clientSecret });
    await expect(registry.register({
      name: 'alias-conflict',
      url: 'https://alias-conflict.service-now.com',
      authType: 'oauth',
      grantType: 'password',
      username: 'user',
      clientId: 'cid',
      credentialRef: {
        password,
        passwordRef: credentialRefFor('alias-conflict', 'password'),
        clientSecret: credentialRefFor('alias-conflict', 'client-secret')
      }
    })).rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
    expect(registry.list().map(instance => instance.name)).toEqual(['alias-equal']);
  });

  test('enforces branch-exclusive metadata on new registry writes while allowing custom token URLs', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const rejects = [
      { ...publicInstance('basic-oauth-field'), authType: 'basic', grantType: undefined, username: 'user', clientId: 'cid' },
      { ...publicInstance('cc-username'), grantType: 'client_credentials', username: 'user', credentialRef: credentialRefFor('cc-username', 'client-secret') },
      { ...publicInstance('cc-auth-endpoint'), grantType: 'client_credentials', authorizeUrl: 'https://safe.example/auth', credentialRef: credentialRefFor('cc-auth-endpoint', 'client-secret') },
      { ...publicInstance('password-auth-endpoint'), grantType: 'password', username: 'user', authorizeUrl: 'https://safe.example/auth', credentialRef: { password: credentialRefFor('password-auth-endpoint', 'password'), clientSecret: credentialRefFor('password-auth-endpoint', 'client-secret') } },
      { ...publicInstance('code-username'), username: 'user' }
    ];
    for (const instance of rejects) {
      await expect(registry.register(instance)).rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
    }
    await expect(registry.register({
      name: 'cc-token-url',
      url: 'https://cc-token-url.service-now.com',
      authType: 'oauth',
      grantType: 'client_credentials',
      clientId: 'cid',
      tokenUrl: 'https://cc-token-url.service-now.com/custom-token',
      credentialRef: credentialRefFor('cc-token-url', 'client-secret')
    })).resolves.toEqual(expect.objectContaining({ name: 'cc-token-url' }));
  });

  test('returns defensive redacted clones from load, document, and reload', () => {
    const { file } = tempPaths();
    writeJson(file, {
      instances: [{
        name: 'legacy',
        url: 'https://legacy.service-now.com',
        username: 'legacy-user',
        password: 'legacy-password-fixture',
        default: true
      }]
    });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    const loaded = registry.load();
    loaded.instances[0].name = 'mutated';
    loaded.instances.push({ name: 'injected' });
    const documented = registry.document;
    documented.instances[0].username = 'mutated';
    const reloaded = registry.reload();
    reloaded.instances[0].name = 'mutated-again';

    expect(registry.list().map(instance => instance.name)).toEqual(['legacy']);
    expect(registry.get('legacy').username).toBe('legacy-user');
    expect(loaded).not.toBe(reloaded);
    expect(documented).not.toBe(loaded);
    expect(reloaded.instances[0]).not.toHaveProperty('password');
  });
  test('uses one stable reload error contract for invalid persisted documents', () => {
    const { file } = tempPaths();
    writeJson(file, {
      version: 1,
      instances: [{ ...publicInstance('broken'), clientId: 42 }]
    });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    expect(() => registry.load()).toThrow(expect.objectContaining({ code: 'REGISTRY_RELOAD_FAILED' }));
    expect(() => registry.reload()).toThrow(expect.objectContaining({ code: 'REGISTRY_RELOAD_FAILED' }));
  });

  test('reloads a changed file and rolls back to the prior snapshot on failure', () => {
    const { file } = tempPaths();
    writeJson(file, {
      version: 1,
      instances: [publicInstance('dev', { default: true })]
    });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    expect(registry.list().map(instance => instance.name)).toEqual(['dev']);

    writeJson(file, {
      version: 1,
      instances: [publicInstance('prod', { default: true })]
    });
    expect(registry.reload().instances.map(instance => instance.name)).toEqual(['prod']);
    const previous = registry.list();
    const before = fs.readFileSync(file, 'utf8');

    writeJson(file, {
      version: 1,
      instances: [
        publicInstance('prod', { default: true }),
        publicInstance('broken', { default: true })
      ]
    });
    expect(() => registry.reload()).toThrow(
      expect.objectContaining({ code: 'REGISTRY_RELOAD_FAILED' })
    );
    expect(registry.list()).toEqual(previous);
    expect(fs.readFileSync(file, 'utf8')).not.toBe(before);
  });

  test('returns typed missing-instance errors for get, update, and remove', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    expect(() => registry.get('missing')).toThrow(expect.objectContaining({
      code: 'INSTANCE_NOT_FOUND',
      details: { name: 'missing' }
    }));
    await expect(registry.update('missing', {})).rejects.toMatchObject({
      code: 'INSTANCE_NOT_FOUND',
      details: { name: 'missing' }
    });
    await expect(registry.remove('missing')).rejects.toMatchObject({
      code: 'INSTANCE_NOT_FOUND',
      details: { name: 'missing' }
    });
  });

  test('preserves foreign temp files after ten atomic open collisions', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await registry.register(publicInstance('dev'));
    const before = fs.readFileSync(file, 'utf8');
    const previous = registry.list();
    const open = jest.spyOn(fs.promises, 'open').mockImplementation(async (tempPath) => {
      fs.writeFileSync(tempPath, 'foreign-temp');
      const error = new Error('fixture collision');
      error.code = 'EEXIST';
      throw error;
    });

    await expect(registry.register(publicInstance('prod'))).rejects.toMatchObject({
      code: 'REGISTRY_WRITE_FAILED'
    });
    expect(open).toHaveBeenCalledTimes(10);
    const tempFiles = fs.readdirSync(path.dirname(file))
      .filter(name => name.startsWith('.instances.json.') && name.endsWith('.tmp'));

    expect(tempFiles).toHaveLength(10);
    for (const tempFile of tempFiles) {
      expect(fs.readFileSync(path.join(path.dirname(file), tempFile), 'utf8')).toBe('foreign-temp');
    }
    expect(registry.list()).toEqual(previous);
    expect(fs.readFileSync(file, 'utf8')).toBe(before);
  });

  test('creates the registry directory with mode 0700 and temp files with mode 0600', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const mkdir = jest.spyOn(fs.promises, 'mkdir');
    const open = jest.spyOn(fs.promises, 'open');

    await registry.register(publicInstance('dev'));

    expect(mkdir).toHaveBeenCalledWith(path.dirname(file), { recursive: true, mode: 0o700 });
    expect(open).toHaveBeenCalledWith(expect.any(String), 'wx', 0o600);
  });

  test('preserves the prior snapshot and file when an atomic rename fails', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await registry.register(publicInstance('dev'));
    const before = fs.readFileSync(file, 'utf8');
    const previous = registry.list();
    jest.spyOn(fs.promises, 'rename').mockRejectedValueOnce(new Error('fixture rename failure'));

    await expect(registry.register(publicInstance('prod')))
      .rejects.toMatchObject({ code: 'REGISTRY_WRITE_FAILED' });
    expect(registry.list()).toEqual(previous);
    expect(fs.readFileSync(file, 'utf8')).toBe(before);
  });
 
  test('write failures expose a stable code and resolved recovery path without source details', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await registry.register(publicInstance('dev'));
    jest.spyOn(fs.promises, 'rename').mockRejectedValueOnce(new Error('fixture-secret-value'));

    let thrown;
    try {
      await registry.register(publicInstance('prod'));
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toMatchObject({
      code: 'REGISTRY_WRITE_FAILED',
      details: { path: path.resolve(file) }
    });
    expect(thrown.message).not.toContain('fixture-secret-value');
    expect(thrown.details).toEqual({ path: path.resolve(file) });
  });

  test('serializes concurrent registrations without losing updates', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    await Promise.all([
      registry.register(publicInstance('one')),
      registry.register(publicInstance('two')),
      registry.register(publicInstance('three'))
    ]);

    expect(registry.list().map(instance => instance.name).sort()).toEqual(['one', 'three', 'two']);
    expect(JSON.parse(fs.readFileSync(file, 'utf8')).instances).toHaveLength(3);
  });

  test('serializes registrations across registry objects sharing a write path', async () => {
    const { file } = tempPaths();
    const first = new InstanceRegistry({ readPath: file, writePath: file });
    const second = new InstanceRegistry({ readPath: file, writePath: file });

    await Promise.all([
      first.register(publicInstance('first')),
      second.register(publicInstance('second'))
    ]);

    expect(JSON.parse(fs.readFileSync(file, 'utf8')).instances
      .map(instance => instance.name)
      .sort()).toEqual(['first', 'second']);
  });

  test('writes a user registry without mutating a distinct legacy read path', async () => {
    const { legacyFile, userFile } = tempPaths();
    writeJson(legacyFile, { version: 1, docs: { keep: true }, instances: [publicInstance('legacy')] });
    const before = fs.readFileSync(legacyFile, 'utf8');
    const registry = new InstanceRegistry({ readPath: legacyFile, writePath: userFile });

    await registry.register(publicInstance('new'));

    expect(fs.readFileSync(legacyFile, 'utf8')).toBe(before);
    expect(JSON.parse(fs.readFileSync(userFile, 'utf8')).docs.keep).toBe(true);
    expect(JSON.parse(fs.readFileSync(userFile, 'utf8')).instances).toHaveLength(2);
  });

  test('updates and removes instances while preserving document properties', async () => {
    const { file } = tempPaths();
    writeJson(file, { version: 1, docs: { enabled: true }, instances: [publicInstance('dev')] });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });

    await registry.update('dev', { description: 'Development', default: true });
    expect(registry.get('dev').description).toBe('Development');
    expect(registry.document.docs.enabled).toBe(true);
    await registry.remove('dev');
    expect(registry.list()).toEqual([]);
    expect(registry.document.docs.enabled).toBe(true);
  });
  test('runs registration precommit validation after the latest reload and before writing', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const precommit = jest.fn(async document => {
      expect(document.instances.map(instance => instance.name)).toEqual(['dev']);
      expect(fs.existsSync(file)).toBe(false);
    });

    await registry.register(publicInstance('dev'), { precommit });

    expect(precommit).toHaveBeenCalledTimes(1);
    expect(registry.list().map(instance => instance.name)).toEqual(['dev']);
  });

  test('does not write when registration precommit validation rejects', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    const precommit = jest.fn(async () => {
      throw Object.assign(new Error('credential disappeared'), {
        code: 'CREDENTIAL_NOT_FOUND'
      });
    });

    await expect(registry.register(publicInstance('dev'), { precommit }))
      .rejects.toMatchObject({ code: 'CREDENTIAL_NOT_FOUND' });
    expect(registry.list()).toEqual([]);
    expect(fs.existsSync(file)).toBe(false);
  });
  test('compensates a just-created default registration and restores the prior default atomically', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await registry.register(publicInstance('old'));
    const priorDefault = registry.getDefault();
    const registered = await registry.register(publicInstance('new'), { makeDefault: true });
    const writeAtomic = jest.spyOn(registry, '_writeAtomic');

    await expect(registry.compensateRegistration('new', {
      expected: registered,
      priorDefault: { name: priorDefault.name, default: priorDefault.default }
    })).resolves.toBeUndefined();

    expect(writeAtomic).toHaveBeenCalledTimes(1);
    expect(registry.list()).toEqual([
      expect.objectContaining({ name: 'old', default: true })
    ]);
  });

  test('does not clobber a concurrently changed registration during compensation', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await registry.register(publicInstance('old'));
    const registered = await registry.register(publicInstance('new'), { makeDefault: true });
    await registry.update('new', { description: 'changed concurrently' });

    await expect(registry.compensateRegistration('new', {
      expected: registered,
      priorDefault: { name: 'old', default: true }
    })).rejects.toMatchObject({ code: 'REGISTRY_ROLLBACK_REQUIRED' });
    expect(registry.get('new')).toEqual(expect.objectContaining({ description: 'changed concurrently', default: true }));
    expect(registry.get('old').default).toBe(false);
  });
});


test('exposes typed registry errors with stable details', () => {
  const error = new InstanceRegistryError('INVALID_INSTANCE_CONFIG', 'invalid name', { field: 'name' });
  expect(error).toBeInstanceOf(Error);
  expect(error.name).toBe('InstanceRegistryError');
  expect(error.code).toBe('INVALID_INSTANCE_CONFIG');
  expect(error.details).toEqual({ field: 'name' });
});

describe('InstanceRegistry OAuth endpoint origin policy', () => {
  const TRUSTED_ENV = 'SERVICENOW_OAUTH_TRUSTED_ORIGINS';
  let saved;
  beforeEach(() => {
    saved = process.env[TRUSTED_ENV];
    delete process.env[TRUSTED_ENV];
  });
  afterEach(() => {
    if (saved === undefined) delete process.env[TRUSTED_ENV];
    else process.env[TRUSTED_ENV] = saved;
  });

  test('accepts same-origin endpoints and rejects unapproved external authorize/token origins at registration', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await expect(registry.register(publicInstance('same-origin', {
      authorizeUrl: 'https://same-origin.service-now.com/oauth_auth.do',
      tokenUrl: 'https://same-origin.service-now.com/oauth_token.do'
    }))).resolves.toEqual(expect.objectContaining({ name: 'same-origin' }));

    for (const [field, value] of [
      ['tokenUrl', 'https://idp.example.com/token'],
      ['authorizeUrl', 'https://idp.example.com/authorize'],
      ['tokenUrl', 'https://same-origin.service-now.com.attacker.example/oauth_token.do']
    ]) {
      await expect(registry.register(publicInstance(`external-${field}`, { [field]: value })))
        .rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG', details: { field } });
    }
    await expect(registry.register({
      name: 'cc-external',
      url: 'https://cc-external.service-now.com',
      authType: 'oauth',
      grantType: 'client_credentials',
      clientId: 'cid',
      tokenUrl: 'https://idp.example.com/token',
      credentialRef: credentialRefFor('cc-external', 'client-secret')
    })).rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG', details: { field: 'tokenUrl' } });
    expect(registry.list().map(instance => instance.name)).toEqual(['same-origin']);
  });

  test('accepts external IdP origins only from the operator allow-list', async () => {
    process.env[TRUSTED_ENV] = 'https://idp.example.com';
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    await expect(registry.register(publicInstance('approved', {
      authorizeUrl: 'https://idp.example.com/authorize',
      tokenUrl: 'https://idp.example.com/token'
    }))).resolves.toEqual(expect.objectContaining({ name: 'approved' }));
    await expect(registry.register(publicInstance('other-port', { tokenUrl: 'https://idp.example.com:8443/token' })))
      .rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG', details: { field: 'tokenUrl' } });
  });

  test('fails registry load when a stored endpoint is no longer approved', () => {
    const { file } = tempPaths();
    writeJson(file, { version: 1, instances: [publicInstance('stored', { tokenUrl: 'https://idp.example.com/token' })] });
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    expect(() => registry.load()).toThrow(expect.objectContaining({ code: 'REGISTRY_RELOAD_FAILED' }));

    process.env[TRUSTED_ENV] = 'https://idp.example.com';
    expect(registry.load().instances[0].tokenUrl).toBe('https://idp.example.com/token');
  });

  test('rejects malformed trusted-origin configuration rather than widening trust', async () => {
    const { file } = tempPaths();
    const registry = new InstanceRegistry({ readPath: file, writePath: file });
    for (const value of ['*', 'https://idp.example.com/path', 'http://idp.example.com', 'https://user:pw@idp.example.com', 'https://idp.example.com?x=1', 'nonsense']) {
      process.env[TRUSTED_ENV] = value;
      await expect(registry.register(publicInstance('bad-allow-list', { tokenUrl: 'https://idp.example.com/token' })))
        .rejects.toMatchObject({ code: 'INVALID_INSTANCE_CONFIG' });
    }
  });
});
