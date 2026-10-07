import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { InstanceRegistry } from '../src/instance-registry.js';
import { afterEach, beforeEach, describe, expect, jest, test } from '@jest/globals';
import { runInstanceCli } from '../src/instance-cli.js';
import { oauthRefreshTokenAccount } from '../src/servicenow-client.js';
import { FileTokenStore, InMemoryTokenStore, KeychainTokenStore } from '../src/token-store.js';

function streams() {
  const out = { chunks: [], write(value) { this.chunks.push(String(value)); } };
  const err = { chunks: [], write(value) { this.chunks.push(String(value)); } };
  return { out, err };
}

function prompts(values = {}) {
  return {
    input: jest.fn(async ({ name, default: fallback }) => values[name] ?? fallback ?? ''),
    password: jest.fn(async ({ name }) => values[name] ?? 'fixture-secret-value'),
    select: jest.fn(async ({ name, default: fallback, choices }) => values[name] ?? fallback ?? choices?.[0]?.value),
    confirm: jest.fn(async ({ name, default: fallback }) => values[name] ?? fallback ?? true)
  };
}

function registryWith(instances = []) {
  const state = instances.map(instance => ({ ...instance }));
  return {
    list: jest.fn(() => state.map(instance => ({ ...instance }))),
    get: jest.fn(name => {
      const instance = state.find(candidate => candidate.name === name);
      if (!instance) throw Object.assign(new Error(`Instance '${name}' not found`), { code: 'INSTANCE_NOT_FOUND' });
      return { ...instance };
    }),
    register: jest.fn(async instance => {
      state.push({ ...instance });
      return { ...instance };
    }),
    update: jest.fn(async (name, patch) => {
      const index = state.findIndex(instance => instance.name === name);
      if (index < 0) throw Object.assign(new Error('missing'), { code: 'INSTANCE_NOT_FOUND' });
      state[index] = { ...state[index], ...patch };
      return { ...state[index] };
    }),
    remove: jest.fn(async name => {
      const index = state.findIndex(instance => instance.name === name);
      if (index < 0) throw Object.assign(new Error('missing'), { code: 'INSTANCE_NOT_FOUND' });
      state.splice(index, 1);
    }),
    validate: jest.fn(() => true),
    _state: state
  };
}

function credentialStore() {
  const values = new Map();
  return {
    values,
    hasSecret: jest.fn(async ref => values.has(ref)),
    setSecret: jest.fn(async (ref, value) => { values.set(ref, value); return { stored: true }; }),
    getSecret: jest.fn(async ref => {
      if (!values.has(ref)) throw Object.assign(new Error('missing'), { code: 'CREDENTIAL_NOT_FOUND' });
      return values.get(ref);
    }),
    deleteSecret: jest.fn(async ref => { values.delete(ref); return { deleted: true }; })
  };
}

const basic = {
  name: 'dev', url: 'https://dev.service-now.com', authType: 'basic', username: 'developer',
  credentialRef: 'keychain:instance/dev/password', default: true
};

describe('runInstanceCli', () => {
  test('lists redacted metadata and prints an empty-registry message', async () => {
    const { out, err } = streams();
    const registry = registryWith([{ ...basic, password: 'must-not-appear' }]);
    const code = await runInstanceCli(['instance', 'list'], { registry, stdout: out, stderr: err });
    expect(code).toBe(0);
    expect(out.chunks.join('')).toContain('dev');
    expect(out.chunks.join('')).not.toContain('must-not-appear');

    const emptyOut = streams();
    const empty = registryWith([]);
    expect(await runInstanceCli(['instance', 'list'], { registry: empty, stdout: emptyOut.out, stderr: emptyOut.err })).toBe(0);
    expect(emptyOut.out.chunks.join('')).toContain('No registered instances');
  });

  test('credential set uses a masked prompt and deterministic reference', async () => {
    const { out, err } = streams();
    const registry = registryWith([{ ...basic, credentialRef: undefined }]);
    const store = credentialStore();
    const prompt = prompts({ secret: 'fixture-secret-value' });
    const code = await runInstanceCli(['instance', 'credential', 'set', 'dev', '--type', 'password'], {
      registry, credentialStore: store, prompts: prompt, stdout: out, stderr: err
    });
    expect(code).toBe(0);
    expect(prompt.password).toHaveBeenCalledWith(expect.objectContaining({ mask: expect.any(String) }));
    expect(store.setSecret).toHaveBeenCalledWith('keychain:instance/dev/password', 'fixture-secret-value');
    expect(registry.update).toHaveBeenCalledWith('dev', expect.objectContaining({ credentialRef: 'keychain:instance/dev/password' }));
    expect(out.chunks.join('')).not.toContain('fixture-secret-value');
  });

  test('add stores credential before metadata and rolls back a newly-created secret on registry failure', async () => {
    const { out, err } = streams();
    const registry = registryWith([]);
    registry.register.mockRejectedValueOnce(Object.assign(new Error('registry unavailable'), { code: 'REGISTRY_WRITE_FAILED', details: { path: '/tmp/registry.json' } }));
    const store = credentialStore();
    const prompt = prompts({ name: 'dev', url: 'https://dev.service-now.com', authType: 'basic', username: 'developer' });
    const code = await runInstanceCli(['instance', 'add'], { registry, credentialStore: store, prompts: prompt, stdout: out, stderr: err });
    expect(code).toBe(1);
    expect(store.setSecret.mock.invocationCallOrder[0]).toBeLessThan(registry.register.mock.invocationCallOrder[0]);
    expect(store.deleteSecret).toHaveBeenCalledWith('keychain:instance/dev/password');
    expect(err.chunks.join('')).toContain('REGISTRY_WRITE_FAILED');
    expect(err.chunks.join('')).toContain('path=/tmp/registry.json');
  });

  test('preflights duplicate and validation before writing deterministic secrets', async () => {
    const { out, err } = streams();
    const registry = registryWith([]);
    registry.validate.mockImplementationOnce(() => true);
    const store = credentialStore();
    const prompt = prompts({ name: 'new-instance', url: 'https://new.service-now.com', authType: 'basic', username: 'developer' });
    expect(await runInstanceCli(['instance', 'add'], {
      registry, credentialStore: store, prompts: prompt, stdout: out, stderr: err
    })).toBe(0);
    expect(registry.validate).toHaveBeenCalledWith(expect.objectContaining({
      name: 'new-instance',
      credentialRef: 'keychain:instance/new-instance/password'
    }));
    expect(registry.validate.mock.invocationCallOrder[0]).toBeLessThan(store.setSecret.mock.invocationCallOrder[0]);
  });

  test('restores an overwritten deterministic secret when registration fails', async () => {
    const { out, err } = streams();
    const registry = registryWith([]);
    registry.register.mockRejectedValueOnce(Object.assign(new Error('registry unavailable'), { code: 'REGISTRY_WRITE_FAILED', details: { path: '/tmp/registry.json' } }));
    const store = credentialStore();
    store.values.set('keychain:instance/dev/password', 'previous-secret');
    const prompt = prompts({ name: 'dev', url: 'https://dev.service-now.com', authType: 'basic', username: 'developer' });
    expect(await runInstanceCli(['instance', 'add'], {
      registry, credentialStore: store, prompts: prompt, stdout: out, stderr: err
    })).toBe(1);
    expect(store.values.get('keychain:instance/dev/password')).toBe('previous-secret');
    expect(store.setSecret).toHaveBeenLastCalledWith('keychain:instance/dev/password', 'previous-secret');
    expect(err.chunks.join('')).toContain('REGISTRY_WRITE_FAILED');
    expect(err.chunks.join('')).toContain('path=/tmp/registry.json');
  });

  test('reports credential rollback failure without replacing the registration cause', async () => {
    const { out, err } = streams();
    const registry = registryWith([]);
    registry.register.mockRejectedValueOnce(Object.assign(new Error('registry unavailable'), { code: 'REGISTRY_WRITE_FAILED', details: { path: '/tmp/registry.json' } }));
    const store = credentialStore();
    store.deleteSecret.mockRejectedValueOnce(new Error('keychain unavailable'));
    const prompt = prompts({ name: 'new-instance', url: 'https://new.service-now.com', authType: 'basic', username: 'developer' });
    expect(await runInstanceCli(['instance', 'add'], {
      registry, credentialStore: store, prompts: prompt, stdout: out, stderr: err
    })).toBe(1);
    expect(err.chunks.join('')).toContain('REGISTRY_WRITE_FAILED');
    expect(err.chunks.join('')).toContain('path=/tmp/registry.json');
    expect(err.chunks.join('')).toContain('rollback failed');
    expect(err.chunks.join('')).not.toContain('fixture-secret-value');
  });

  test('rejects duplicate names before prompting for or writing a secret', async () => {
    const { out, err } = streams();
    const registry = registryWith([basic]);
    const store = credentialStore();
    const prompt = prompts({ name: 'dev', url: 'https://dev.service-now.com', authType: 'basic', username: 'developer' });
    expect(await runInstanceCli(['instance', 'add'], {
      registry, credentialStore: store, prompts: prompt, stdout: out, stderr: err
    })).toBe(1);
    expect(store.setSecret).not.toHaveBeenCalled();
    expect(prompt.password).not.toHaveBeenCalled();
    expect(err.chunks.join('')).toContain('already exists');
  });

  test('rejects secret flags before prompts or output', async () => {
    const { out, err } = streams();
    const prompt = prompts();
    const code = await runInstanceCli(['instance', 'update', 'dev', '--password', 'fixture-secret-value'], {
      registry: registryWith([basic]), prompts: prompt, stdout: out, stderr: err
    });
    expect(code).toBe(2);
    expect(prompt.input).not.toHaveBeenCalled();
    expect(prompt.password).not.toHaveBeenCalled();
    expect(out.chunks.join('')).not.toContain('fixture-secret-value');
    expect(err.chunks.join('')).not.toContain('fixture-secret-value');
  });
 
test.each([
  ['--password=fixture-secret-value'],
  ['password=fixture-secret-value'],
  ['--clientSecret=fixture-secret-value'],
  ['client-secret=fixture-secret-value'],
  ['client_secret=fixture-secret-value'],
  ['--CLIENT_SECRET=fixture-secret-value'],
  ['--type=fixture-secret-value'],
  ['--type', 'fixture-secret-value']
])('rejects secret assignment %j before parsing', async (...tokens) => {
  const { out, err } = streams();
  const prompt = prompts();
  const registry = registryWith([basic]);
  expect(await runInstanceCli(['instance', 'credential', 'set', 'dev', ...tokens], {
    registry, prompts: prompt, stdout: out, stderr: err
  })).toBe(2);
  expect(err.chunks.join('')).toBe('Secret flags and values are not accepted in command arguments; use a masked prompt.\n');
  expect(err.chunks.join('')).not.toContain('fixture-secret-value');
  expect(out.chunks.join('')).not.toContain('fixture-secret-value');
  expect(registry.get).not.toHaveBeenCalled();
});

test('unknown credential type uses constant safe usage text', async () => {
  const { out, err } = streams();
  expect(await runInstanceCli(['instance', 'credential', 'set', 'dev', '--type', 'fixture-secret-value'], {
    registry: registryWith([basic]), prompts: prompts(), stdout: out, stderr: err
  })).toBe(2);
  expect(err.chunks.join('')).not.toContain('fixture-secret-value');
  expect(err.chunks.join('')).toBe('Secret flags and values are not accepted in command arguments; use a masked prompt.\n');
});

test('registry write failures include only stable code and resolved path', async () => {
  const { out, err } = streams();
  const registry = registryWith([]);
  registry.register.mockRejectedValueOnce(Object.assign(new Error('fixture-secret-value'), {
    code: 'REGISTRY_WRITE_FAILED',
    details: { path: './fixture-registry.json', secret: 'fixture-secret-value', arbitrary: 'do-not-echo' }
  }));
  const code = await runInstanceCli(['instance', 'add'], {
    registry,
    credentialStore: credentialStore(),
    prompts: prompts({ name: 'dev', url: 'https://dev.service-now.com', authType: 'basic', username: 'developer' }),
    stdout: out,
    stderr: err
  });
  const text = err.chunks.join('');
  expect(code).toBe(1);
  expect(text).toContain('REGISTRY_WRITE_FAILED');
  expect(text).toContain('fixture-registry.json');
  expect(text).not.toContain('fixture-secret-value');
  expect(text).not.toContain('arbitrary');
});

test('unknown instance commands use constant safe usage text', async () => {
  const { out, err } = streams();
  expect(await runInstanceCli(['instance', 'fixture-secret-value'], { stdout: out, stderr: err })).toBe(2);
  expect(err.chunks.join('')).not.toContain('fixture-secret-value');
  expect(err.chunks.join('')).toContain('Usage:');
});

  test('remove confirms before metadata and referenced secret deletion', async () => {
    const { out, err } = streams();
    const registry = registryWith([basic]);
    const store = credentialStore();
    const prompt = prompts({ confirm: false });
    expect(await runInstanceCli(['instance', 'remove', 'dev'], { registry, credentialStore: store, prompts: prompt, stdout: out, stderr: err })).toBe(0);
    expect(registry.remove).not.toHaveBeenCalled();
    expect(store.deleteSecret).not.toHaveBeenCalled();
  });

  test('test reports success, authentication, and authorization failures distinctly', async () => {
    const registry = registryWith([basic]);
    const store = credentialStore();
    const out1 = streams();
    const successClient = { getRecords: jest.fn(async () => []) };
    expect(await runInstanceCli(['instance', 'test', 'dev'], {
      registry, credentialStore: store, clientFactory: jest.fn(() => successClient), stdout: out1.out, stderr: out1.err
    })).toBe(0);
    expect(out1.out.chunks.join('')).toContain('succeeded');

    for (const [status, label] of [[401, 'Authentication'], [403, 'Authorization']]) {
      const result = streams();
      const clientFactory = jest.fn(() => ({ getRecords: jest.fn(async () => { throw Object.assign(new Error('request failed'), { response: { status } }); }) }));
      expect(await runInstanceCli(['instance', 'test', 'dev'], {
        registry, credentialStore: store, clientFactory, stdout: result.out, stderr: result.err

      })).toBe(1);
      expect(result.err.chunks.join('')).toContain(label);
    }
  });

test('remove restores deleted secrets and leaves metadata when a later deletion fails', async () => {
  const { out, err } = streams();
  const instance = {
    ...basic,
    credentialRef: { password: 'keychain:instance/dev/password', clientSecret: 'keychain:instance/dev/client-secret' },
    authType: 'oauth',
    grantType: 'password',
    clientId: 'client',
    username: 'developer'
  };
  const registry = registryWith([instance]);
  const store = credentialStore();
  store.values.set('keychain:instance/dev/password', 'old-password');
  store.values.set('keychain:instance/dev/client-secret', 'old-client-secret');
  store.deleteSecret.mockImplementationOnce(async ref => {
    store.values.delete(ref);
    return { deleted: true };
  }).mockRejectedValueOnce(new Error('keychain unavailable'));
  const code = await runInstanceCli(['instance', 'remove', 'dev'], {
    registry, credentialStore: store, prompts: prompts({ confirm: true }), stdout: out, stderr: err
  });
  expect(code).toBe(1);
  expect(registry.remove).not.toHaveBeenCalled();
  expect(store.values.get('keychain:instance/dev/password')).toBe('old-password');
  expect(err.chunks.join('')).toContain('rollback');
});

test('remove restores all secrets when metadata removal fails', async () => {
  const { out, err } = streams();
  const registry = registryWith([basic]);
  registry.remove.mockRejectedValueOnce(Object.assign(new Error('registry unavailable'), { code: 'REGISTRY_WRITE_FAILED', details: { path: '/tmp/registry.json' } }));
  const store = credentialStore();
  store.values.set(basic.credentialRef, 'old-password');
  const code = await runInstanceCli(['instance', 'remove', 'dev'], {
    registry, credentialStore: store, prompts: prompts({ confirm: true }), stdout: out, stderr: err
  });
  expect(code).toBe(1);
  expect(store.values.get(basic.credentialRef)).toBe('old-password');
  expect(err.chunks.join('')).toContain('REGISTRY_WRITE_FAILED');
  expect(err.chunks.join('')).toContain('path=/tmp/registry.json');
});

  test('updates only allowed metadata flags without prompting for secrets', async () => {
    const { out, err } = streams();
    const registry = registryWith([basic]);
    const prompt = prompts();
    expect(await runInstanceCli(['instance', 'update', 'dev', '--description', 'Updated'], {
      registry, prompts: prompt, stdout: out, stderr: err
    })).toBe(0);
    expect(registry.update).toHaveBeenCalledWith('dev', { description: 'Updated' });
    expect(prompt.password).not.toHaveBeenCalled();
  });
  test.each([
    ['auth-type', 'oauth'],
    ['grant-type', 'client_credentials']
  ])('rejects --%s changes before registry mutation with re-add guidance', async (flag, value) => {
    const { out, err } = streams();
    const registry = registryWith([basic]);
    const code = await runInstanceCli(['instance', 'update', 'dev', `--${flag}`, value], {
      registry, prompts: prompts(), stdout: out, stderr: err
    });
    expect(code).toBe(2);
    expect(registry.update).not.toHaveBeenCalled();
    expect(err.chunks.join('')).toMatch(/auth(?:entication)? changes require remove\/re-add/i);
  });
  test('uses the inferred password grant for credential choices and preserves the canonical object when setting either credential', async () => {
    const { out, err } = streams();
    const refs = {
      password: 'keychain:instance/inferred/password',
      clientSecret: 'keychain:instance/inferred/client-secret'
    };
    const instance = {
      name: 'inferred',
      url: 'https://inferred.service-now.com',
      authType: 'oauth',
      clientId: 'client',
      username: 'developer',
      credentialRef: { clientSecret: refs.clientSecret }
    };
    const registry = registryWith([instance]);
    const store = credentialStore();
    const prompt = prompts({ secret: 'new-password' });

    expect(await runInstanceCli(['instance', 'credential', 'set', 'inferred', '--type', 'password'], {
      registry, credentialStore: store, prompts: prompt, stdout: out, stderr: err
    })).toBe(0);
    expect(registry.update).toHaveBeenCalledWith('inferred', {
      credentialRef: { password: refs.password, clientSecret: refs.clientSecret }
    });

    registry.update.mockClear();
    prompt.password.mockClear();
    prompt.password.mockResolvedValueOnce('new-client-secret');
    expect(await runInstanceCli(['instance', 'credential', 'set', 'inferred', '--type', 'client-secret'], {
      registry, credentialStore: store, prompts: prompt, stdout: out, stderr: err
    })).toBe(0);
    expect(registry.update).toHaveBeenCalledWith('inferred', {
      credentialRef: { password: refs.password, clientSecret: refs.clientSecret }
    });
  });

  test.each(['12junk', '12.5', '1e2', '0x10', '+12', '-1', ''])(
    'rejects invalid add redirect port %j before registry validation',
    async value => {
      const { out, err } = streams();
      const registry = registryWith([]);
      const code = await runInstanceCli([
        'instance', 'add', '--name', 'redirect', '--url', 'https://redirect.service-now.com',
        '--auth-type', 'oauth', '--grant-type', 'authorization_code', '--client-id', 'client',
        '--redirect-port', value
      ], { registry, prompts: prompts(), stdout: out, stderr: err });
      expect(err.chunks.join('')).toMatch(/redirect[- ]port/i);
      expect(registry.validate).not.toHaveBeenCalled();
    }
  );

  test('normalizes a valid add redirect port before validation and persistence', async () => {
    const { out, err } = streams();
    const registry = registryWith([]);
    expect(await runInstanceCli([
      'instance', 'add', '--name', 'redirect', '--url', 'https://redirect.service-now.com',
      '--auth-type', 'oauth', '--grant-type', 'authorization_code', '--client-id', 'client',
      '--redirect-port', '8455'
    ], { registry, prompts: prompts(), stdout: out, stderr: err })).toBe(0);
    expect(registry.validate).toHaveBeenCalledWith(expect.objectContaining({ redirectPort: 8455 }));
    expect(registry.register).toHaveBeenCalledWith(expect.objectContaining({ redirectPort: 8455 }), expect.anything());
  });

  test.each(['12junk', '12.5', '1e2', '0x10', '+12', '-1', ''])(
    'rejects invalid update redirect port %j before registry update',
    async value => {
      const { out, err } = streams();
      const registry = registryWith([basic]);
      const code = await runInstanceCli(['instance', 'update', 'dev', '--redirect-port', value], {
        registry, prompts: prompts(), stdout: out, stderr: err
      });
      expect(err.chunks.join('')).toMatch(/redirect[- ]port/i);
      expect(registry.update).not.toHaveBeenCalled();
    }
  );

  test('normalizes a valid update redirect port before registry update', async () => {
    const { out, err } = streams();
    const registry = registryWith([basic]);
    expect(await runInstanceCli(['instance', 'update', 'dev', '--redirect-port', '8455'], {
      registry, prompts: prompts(), stdout: out, stderr: err
    })).toBe(0);
    expect(registry.update).toHaveBeenCalledWith('dev', { redirectPort: 8455 });
  });


  test('unknown commands return usage code 2', async () => {
    const { out, err } = streams();
    expect(await runInstanceCli(['instance', 'wat'], { stdout: out, stderr: err })).toBe(2);
    expect(err.chunks.join('')).toContain('Usage:');
  });
});

test('migrate stores plaintext credentials as refs, preserves document properties, and is idempotent', async () => {
  const legacy = {
    version: 1,
    docs: { localIndexEnabled: true },
    custom: { keep: true },
    instances: [
      { name: 'dev', url: 'https://dev.service-now.com', username: 'developer', password: 'fixture-password' },
      { name: 'oauth', url: 'https://oauth.service-now.com', authType: 'oauth', grantType: 'password', clientId: 'client', username: 'developer', password: 'fixture-password-2', clientSecret: 'fixture-client-secret' }
    ]
  };
  const registry = {
    source: 'legacy',
    writePath: '/tmp/happy-user-registry.json',
    _rawDocument: jest.fn(() => legacy),
    _writeAtomic: jest.fn(async document => { registry.written = document; }),
    written: null
  };
  const store = credentialStore();
  const result = streams();
  const prompt = prompts({ confirm: true });
  expect(await runInstanceCli(['instance', 'migrate'], { registry, credentialStore: store, prompts: prompt, stdout: result.out, stderr: result.err })).toBe(0);
  expect(registry.written.docs).toEqual({ localIndexEnabled: true });
  expect(registry.written.custom).toEqual({ keep: true });
  expect(registry.written.instances[0]).toEqual(expect.objectContaining({ credentialRef: 'keychain:instance/dev/password' }));
  expect(registry.written.instances[0]).not.toHaveProperty('password');
  expect(registry.written.instances[1].credentialRef).toEqual({
    password: 'keychain:instance/oauth/password',
    clientSecret: 'keychain:instance/oauth/client-secret'
  });
  expect(store.values.size).toBe(3);
  expect(result.out.chunks.join('')).not.toContain('fixture-password');
  expect(result.out.chunks.join('')).not.toContain('fixture-client-secret');
});
test('migrate refuses same-file plaintext source before keychain writes', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-migration-same-file-'));
  const sourcePath = path.join(directory, 'instances.json');
  const source = JSON.stringify({
    version: 1,
    instances: [{
      name: 'dev',
      url: 'https://dev.service-now.com',
      username: 'developer',
      password: 'fixture-password'
    }]
  }, null, 2);
  fs.writeFileSync(sourcePath, source, 'utf8');
  const registry = new InstanceRegistry({ readPath: sourcePath, writePath: sourcePath });
  const store = credentialStore();
  const result = streams();
  const prompt = prompts({ confirm: true });

  try {
    expect(await runInstanceCli(['instance', 'migrate'], {
      registry,
      credentialStore: store,
      prompts: prompt,
      stdout: result.out,
      stderr: result.err
    })).toBe(2);
    expect(fs.readFileSync(sourcePath, 'utf8')).toBe(source);
    expect(store.setSecret).not.toHaveBeenCalled();
    expect(prompt.confirm).not.toHaveBeenCalled();
    expect(result.err.chunks.join('')).toMatch(/same file|distinct .*HAPPY_CONFIG_PATH|automatic.*user-registry/i);
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});
test('migrate refuses symlink aliases before keychain writes', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-migration-symlink-'));
  const sourcePath = path.join(directory, 'instances.json');
  const aliasPath = path.join(directory, 'alias.json');
  fs.writeFileSync(sourcePath, '{}', 'utf8');
  fs.symlinkSync(sourcePath, aliasPath);
  const registry = {
    readPath: sourcePath,
    writePath: aliasPath,
    _rawDocument: () => ({
      instances: [{ name: 'dev', url: 'https://dev.service-now.com', username: 'developer', password: 'fixture-password' }]
    }),
    _writeAtomic: jest.fn(async () => {})
  };
  const store = credentialStore();
  const result = streams();

  try {
    expect(await runInstanceCli(['instance', 'migrate'], {
      registry,
      credentialStore: store,
      prompts: prompts({ confirm: true }),
      stdout: result.out,
      stderr: result.err
    })).toBe(2);
    expect(store.setSecret).not.toHaveBeenCalled();
    expect(registry._writeAtomic).not.toHaveBeenCalled();
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});
test('migrate allows a genuinely nonexistent destination after canonicalizing its parent', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-migration-new-target-'));
  const sourcePath = path.join(directory, 'legacy.json');
  const targetPath = path.join(directory, 'nested', 'instances.json');
  const registry = {
    readPath: sourcePath,
    writePath: targetPath,
    _rawDocument: () => ({
      instances: [{ name: 'dev', url: 'https://dev.service-now.com', username: 'developer', password: 'fixture-password' }]
    }),
    _writeAtomic: jest.fn(async document => { registry.written = document; }),
    written: null
  };
  const store = credentialStore();
  const result = streams();

  try {
    expect(await runInstanceCli(['instance', 'migrate'], {
      registry,
      credentialStore: store,
      prompts: prompts({ confirm: true }),
      stdout: result.out,
      stderr: result.err
    })).toBe(0);
    expect(store.setSecret).toHaveBeenCalledWith('keychain:instance/dev/password', 'fixture-password');
    expect(registry._writeAtomic).toHaveBeenCalledTimes(1);
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

test('migrate preserves missing target component order during identity comparison', async () => {
  const registry = {
    readPath: '/tmp/target/sub',
    writePath: '/tmp/sub/target',
    fs: {
      realpathSync: jest.fn(candidate => {
        if (candidate === '/tmp/target/sub' || candidate === '/tmp') return candidate;
        throw Object.assign(new Error('missing'), { code: 'ENOENT' });
      })
    },
    _rawDocument: () => ({
      instances: [{ name: 'dev', url: 'https://dev.service-now.com', username: 'developer', password: 'fixture-password' }]
    }),
    _writeAtomic: jest.fn(async () => {})
  };
  const store = credentialStore();
  const result = streams();

  expect(await runInstanceCli(['instance', 'migrate'], {
    registry,
    credentialStore: store,
    prompts: prompts({ confirm: true }),
    stdout: result.out,
    stderr: result.err
  })).toBe(0);
  expect(store.setSecret).toHaveBeenCalledWith('keychain:instance/dev/password', 'fixture-password');
  expect(registry._writeAtomic).toHaveBeenCalledTimes(1);
});


test('migrate fails closed when an injected fs adapter cannot prove source identity', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-migration-missing-realpath-'));
  const sourcePath = path.join(directory, 'legacy.json');
  const targetPath = path.join(directory, 'registry.json');
  const source = JSON.stringify({
    version: 1,
    instances: [{
      name: 'dev',
      url: 'https://dev.service-now.com',
      username: 'developer',
      password: 'fixture-secret-value'
    }]
  }, null, 2);
  fs.writeFileSync(sourcePath, source, 'utf8');
  const registry = {
    readPath: sourcePath,
    writePath: targetPath,
    fs: {},
    _rawDocument: () => JSON.parse(source),
    migrateLegacy: jest.fn(async () => {})
  };
  const store = credentialStore();
  const result = streams();

  try {
    expect(await runInstanceCli(['instance', 'migrate'], {
      registry,
      credentialStore: store,
      prompts: prompts({ confirm: true }),
      stdout: result.out,
      stderr: result.err
    })).toBe(2);
    expect(store.setSecret).not.toHaveBeenCalled();
    expect(registry.migrateLegacy).not.toHaveBeenCalled();
    expect(fs.readFileSync(sourcePath, 'utf8')).toBe(source);
    const errorOutput = result.err.chunks.join('');
    expect(errorOutput).toContain('MIGRATION_SOURCE_IDENTITY_FAILED');
    expect(errorOutput).toContain(path.resolve(sourcePath));
    expect(errorOutput).toContain(path.resolve(targetPath));
    expect(errorOutput).not.toContain('fixture-secret-value');
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

test('migrate refuses permission errors while resolving source or target identity', async () => {
  const registry = {
    readPath: '/tmp/plaintext-source.json',
    writePath: '/tmp/user-target.json',
    fs: {
      realpathSync: jest.fn((candidate) => {
        const error = new Error(`cannot inspect ${candidate}`);
        error.code = candidate.endsWith('source.json') ? 'EIO' : 'EACCES';
        throw error;
      })
    },
    _rawDocument: () => ({
      instances: [{ name: 'dev', url: 'https://dev.service-now.com', username: 'developer', password: 'fixture-password' }]
    }),
    _writeAtomic: jest.fn(async () => {})
  };
  const store = credentialStore();
  const result = streams();

  expect(await runInstanceCli(['instance', 'migrate'], {
    registry,
    credentialStore: store,
    prompts: prompts({ confirm: true }),
    stdout: result.out,
    stderr: result.err
  })).toBe(2);
  const errorOutput = result.err.chunks.join('');
  expect(errorOutput).toContain('MIGRATION_SOURCE_IDENTITY_FAILED');
  expect(errorOutput).toContain(path.resolve(registry.readPath));
  expect(errorOutput).toContain(path.resolve(registry.writePath));
  expect(errorOutput).not.toContain('fixture-password');
  expect(store.setSecret).not.toHaveBeenCalled();
  expect(registry._writeAtomic).not.toHaveBeenCalled();
});


test('migrate rolls back only newly-created secrets when registry write fails', async () => {
  const registry = {
    _rawDocument: () => ({ instances: [{ name: 'dev', url: 'https://dev.service-now.com', username: 'developer', password: 'fixture-password' }] }),
    _writeAtomic: jest.fn(async () => { throw new Error('write failed'); })
  };
  const store = credentialStore();
  const result = streams();
  expect(await runInstanceCli(['instance', 'migrate'], {
    registry, credentialStore: store, prompts: prompts({ confirm: true }), stdout: result.out, stderr: result.err
  })).toBe(1);
  expect(store.values.size).toBe(0);
  expect(result.err.chunks.join('')).toContain('write failed');
});
 
test('CLI dispatch delegates server startup arguments to stdio main without starting it for help', async () => {
  const main = jest.fn(async () => {});
  jest.unstable_mockModule('../src/stdio-server.js', () => ({ main }));
  const { dispatch } = await import('../src/cli.js');

  const outputStreams = streams();
  expect(await dispatch([], { stdout: outputStreams.out, stderr: outputStreams.err })).toBe(0);
  expect(main).toHaveBeenCalledTimes(1);
  main.mockClear();
  expect(await dispatch(['--docs-only'], { stdout: outputStreams.out, stderr: outputStreams.err })).toBe(0);
  expect(main).toHaveBeenCalledTimes(1);
  main.mockClear();
  expect(await dispatch(['--help'], { stdout: outputStreams.out, stderr: outputStreams.err })).toBe(0);
  expect(main).not.toHaveBeenCalled();
});

test.each([
  ['oauth', 'authorization_code', { clientId: 'client', clientSecret: 'legacy-secret' }, 'confidential authorization-code'],
  ['oauth', 'password', { clientId: 'client', username: 'developer', password: 'legacy-password' }, 'incomplete password grant'],
  ['basic', undefined, { username: 'developer', password: 'legacy-password', token: 'unexpected-secret' }, 'unknown secret placement']
])('migration rejects %s before keychain writes (%s)', async (authType, grantType, fields) => {
  const legacy = { docs: { keep: true }, instances: [{ name: 'dev', url: 'https://dev.service-now.com', authType, ...(grantType ? { grantType } : {}), ...fields }] };
  const registry = {
    _rawDocument: () => legacy,
    _writeAtomic: jest.fn(async () => {})
  };
  const store = credentialStore();
  const result = streams();
  expect(await runInstanceCli(['instance', 'migrate'], {
    registry, credentialStore: store, prompts: prompts({ confirm: true }), stdout: result.out, stderr: result.err
  })).toBe(1);
  expect(store.setSecret).not.toHaveBeenCalled();
  expect(registry._writeAtomic).not.toHaveBeenCalled();
  expect(result.err.chunks.join('')).not.toContain('legacy-password');
  expect(result.err.chunks.join('')).not.toContain('legacy-secret');
});

test('non-TTY real prompts return guidance without invoking Inquirer', async () => {
  const out = { chunks: [], isTTY: false, write(value) { this.chunks.push(String(value)); } };
  const err = { chunks: [], isTTY: false, write(value) { this.chunks.push(String(value)); } };
  const stdin = { isTTY: false };
  expect(await runInstanceCli(['instance', 'add', '--name', 'dev', '--url', 'https://dev.service-now.com', '--username', 'developer'], {
    registry: registryWith([]), credentialStore: credentialStore(), stdin, stdout: out, stderr: err
  })).toBe(2);
  expect(err.chunks.join('').toLowerCase()).toContain('non-interactive');
  expect(err.chunks.join('')).toContain('masked prompt');
});


test('fully specified public authorization-code add runs without a TTY', async () => {
  const out = { chunks: [], isTTY: false, write(value) { this.chunks.push(String(value)); } };
  const err = { chunks: [], isTTY: false, write(value) { this.chunks.push(String(value)); } };
  const registry = registryWith([]);
  const store = credentialStore();
  const args = [
    'instance', 'add', '--name', 'public', '--url', 'https://public.service-now.com',
    '--auth-type', 'oauth', '--grant-type', 'authorization_code', '--client-id', 'public-client',
    '--scope', 'openid', '--authorize-url', 'https://public.service-now.com/authorize',
    '--token-url', 'https://public.service-now.com/token', '--callback-path', '/oauth/callback',
    '--description', 'public client'
  ];
  expect(await runInstanceCli(args, { registry, credentialStore: store, stdin: { isTTY: false }, stdout: out, stderr: err })).toBe(0);
  expect(registry.register).toHaveBeenCalled();
  expect(store.setSecret).not.toHaveBeenCalled();
});

test.each([
  ['default input', ['instance', 'add'], { password: jest.fn() }],
  ['default select', ['instance', 'credential', 'set', 'dev'], { input: jest.fn() }],
  ['default password', ['instance', 'credential', 'set', 'dev', '--type', 'password'], { input: jest.fn() }],
  ['default confirm', ['instance', 'remove', 'dev'], { input: jest.fn() }]
])('partial prompt injection cannot invoke %s in a non-TTY', async (_label, args, injected) => {
  const { out, err } = streams();
  const registry = registryWith([basic]);
  const code = await runInstanceCli(args, {
    registry,
    credentialStore: credentialStore(),
    prompts: injected,
    stdin: { isTTY: false },
    stdout: { ...out, isTTY: false },
    stderr: { ...err, isTTY: false }
  });

  expect(code).toBe(2);
  for (const prompt of Object.values(injected)) expect(prompt).not.toHaveBeenCalled();
});

describe('OAuth refresh-token identity cleanup', () => {
  const codeInstance = {
    name: 'code', url: 'https://code.service-now.com', authType: 'oauth', grantType: 'authorization_code',
    clientId: 'public-client', default: false
  };
  const legacyAccount = `${os.userInfo().username}@code`;
  const tempDirs = [];
  const savedEnv = {};

  beforeEach(() => {
    for (const key of ['SERVICENOW_TOKEN_STORE', 'XDG_CONFIG_HOME']) savedEnv[key] = process.env[key];
    delete process.env.SERVICENOW_TOKEN_STORE;
  });

  afterEach(() => {
    for (const [key, value] of Object.entries(savedEnv)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
    for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
    jest.restoreAllMocks();
  });

  async function seededTokenStore(entries = []) {
    const store = new InMemoryTokenStore();
    for (const [account, token] of entries) await store.setRefreshToken(account, token);
    jest.spyOn(store, 'clearRefreshToken');
    return store;
  }

  function untouchableTokenStore() {
    const fail = jest.fn(async () => { throw new Error('token store must not be touched'); });
    return { getRefreshToken: fail, setRefreshToken: fail, clearRefreshToken: fail };
  }

  function fakeKeychain(entries = new Map()) {
    return new KeychainTokenStore({
      createEntry: (_service, account) => ({
        getPassword: async () => entries.get(account),
        setPassword: async value => { entries.set(account, value); },
        deletePassword: async () => entries.delete(account)
      })
    });
  }

  function tempConfigHome() {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-cli-tokens-'));
    tempDirs.push(dir);
    return dir;
  }

  test('remove clears the identity and legacy keys from both the keychain and the file store, leaving other identities', async () => {
    const { out, err } = streams();
    const current = oauthRefreshTokenAccount(codeInstance);
    const other = oauthRefreshTokenAccount({ ...codeInstance, url: 'https://other.service-now.com' });
    const seed = [[current, 'rt-current'], [legacyAccount, 'rt-legacy'], [other, 'rt-other']];
    const keychainTokenStore = await seededTokenStore(seed);
    const fileTokenStore = await seededTokenStore(seed);
    const registry = registryWith([codeInstance]);

    expect(await runInstanceCli(['instance', 'remove', 'code'], {
      registry, credentialStore: credentialStore(), keychainTokenStore, fileTokenStore, prompts: prompts({ confirm: true }), stdout: out, stderr: err
    })).toBe(0);

    expect(registry.remove).toHaveBeenCalledWith('code');
    for (const store of [keychainTokenStore, fileTokenStore]) {
      expect(await store.getRefreshToken(current)).toBeNull();
      expect(await store.getRefreshToken(legacyAccount)).toBeNull();
      expect(await store.getRefreshToken(other)).toBe('rt-other');
    }
    expect(out.chunks.join('')).toMatch(/OS keychain, file token store/);
  });

  test('remove deletes a file-store token even when SERVICENOW_TOKEN_STORE is absent from the CLI environment', async () => {
    const { out, err } = streams();
    process.env.XDG_CONFIG_HOME = tempConfigHome();
    const seeded = new FileTokenStore();
    await seeded.setRefreshToken(oauthRefreshTokenAccount(codeInstance), 'rt-left-behind');
    const registry = new InstanceRegistry({ readPath: path.join(tempConfigHome(), 'instances.json'), writePath: path.join(tempConfigHome(), 'instances.json') });
    registry.readPath = registry.writePath;
    await registry.register(codeInstance);

    expect(await runInstanceCli(['instance', 'remove', 'code'], {
      registry, credentialStore: credentialStore(), keychainTokenStore: fakeKeychain(), prompts: prompts({ confirm: true }), stdout: out, stderr: err
    })).toBe(0);

    expect(await new FileTokenStore().getRefreshToken(oauthRefreshTokenAccount(codeInstance))).toBeNull();
    expect(registry.list()).toEqual([]);
  });

  test.each([['keychain'], ['file']])('remove fails closed before deleting anything when the %s store cannot be cleaned', async (failing) => {
    const { out, err } = streams();
    const registry = registryWith([codeInstance]);
    const store = credentialStore();
    const broken = { clearRefreshToken: jest.fn(async () => { throw new Error('store locked'); }) };
    const healthy = await seededTokenStore();

    expect(await runInstanceCli(['instance', 'remove', 'code'], {
      registry,
      credentialStore: store,
      keychainTokenStore: failing === 'keychain' ? broken : healthy,
      fileTokenStore: failing === 'file' ? broken : healthy,
      prompts: prompts({ confirm: true }),
      stdout: out,
      stderr: err
    })).toBe(1);
    expect(registry.remove).not.toHaveBeenCalled();
    expect(store.deleteSecret).not.toHaveBeenCalled();
  });

  test('on Windows the file store is skipped only when its directory does not exist', async () => {
    const platform = Object.getOwnPropertyDescriptor(process, 'platform');
    Object.defineProperty(process, 'platform', { value: 'win32' });
    try {
      const keychainTokenStore = await seededTokenStore([[oauthRefreshTokenAccount(codeInstance), 'rt-keychain']]);
      const missing = new FileTokenStore({ baseDir: path.join(tempConfigHome(), 'absent') });
      const absent = streams();
      expect(await runInstanceCli(['instance', 'remove', 'code'], {
        registry: registryWith([codeInstance]), credentialStore: credentialStore(), keychainTokenStore, fileTokenStore: missing,
        prompts: prompts({ confirm: true }), stdout: absent.out, stderr: absent.err
      })).toBe(0);
      expect(await keychainTokenStore.getRefreshToken(oauthRefreshTokenAccount(codeInstance))).toBeNull();
      expect(absent.out.chunks.join('')).not.toContain('file token store');

      const existing = new FileTokenStore({ baseDir: tempConfigHome() });
      const present = streams();
      const registry = registryWith([codeInstance]);
      expect(await runInstanceCli(['instance', 'remove', 'code'], {
        registry, credentialStore: credentialStore(), keychainTokenStore: await seededTokenStore(), fileTokenStore: existing,
        prompts: prompts({ confirm: true }), stdout: present.out, stderr: present.err
      })).toBe(1);
      expect(registry.remove).not.toHaveBeenCalled();
      expect(present.err.chunks.join('')).toMatch(/keychain on Windows/);
    } finally {
      Object.defineProperty(process, 'platform', platform);
    }
  });

  test.each([
    ['client id', ['--client-id', 'rotated-client']],
    ['instance URL', ['--url', 'https://moved.service-now.com']],
    ['token URL', ['--token-url', 'https://code.service-now.com/alternate_token.do']],
    ['authorize URL', ['--authorize-url', 'https://code.service-now.com/alternate_auth.do']]
  ])('update clears the previous identity from both stores when the %s changes', async (_label, flags) => {
    const { out, err } = streams();
    const previous = oauthRefreshTokenAccount(codeInstance);
    const seed = [[previous, 'rt-previous'], [legacyAccount, 'rt-legacy']];
    const keychainTokenStore = await seededTokenStore(seed);
    const fileTokenStore = await seededTokenStore(seed);
    const registry = registryWith([codeInstance]);

    expect(await runInstanceCli(['instance', 'update', 'code', ...flags], {
      registry, keychainTokenStore, fileTokenStore, prompts: prompts(), stdout: out, stderr: err
    })).toBe(0);

    const updated = registry._state[0];
    expect(oauthRefreshTokenAccount(updated)).not.toBe(previous);
    for (const store of [keychainTokenStore, fileTokenStore]) {
      expect(await store.getRefreshToken(previous)).toBeNull();
      expect(await store.getRefreshToken(legacyAccount)).toBeNull();
      expect(store.clearRefreshToken).not.toHaveBeenCalledWith(oauthRefreshTokenAccount(updated));
    }
    expect(out.chunks.join('')).toMatch(/OS keychain, file token store/);
  });

  test('update fails closed before changing the registry when refresh-token cleanup fails', async () => {
    const { out, err } = streams();
    const registry = registryWith([codeInstance]);
    const broken = { clearRefreshToken: jest.fn(async () => { throw new Error('store locked'); }) };

    expect(await runInstanceCli(['instance', 'update', 'code', '--client-id', 'rotated-client'], {
      registry, keychainTokenStore: await seededTokenStore(), fileTokenStore: broken, prompts: prompts(), stdout: out, stderr: err
    })).toBe(1);
    expect(broken.clearRefreshToken).toHaveBeenCalled();
    expect(registry.update).not.toHaveBeenCalled();
    expect(registry._state[0].clientId).toBe('public-client');
  });

  test('metadata-only updates and basic instances never touch either refresh-token store', async () => {
    const { out, err } = streams();
    const keychainTokenStore = untouchableTokenStore();
    const fileTokenStore = untouchableTokenStore();
    expect(await runInstanceCli(['instance', 'update', 'code', '--description', 'docs only'], {
      registry: registryWith([codeInstance]), keychainTokenStore, fileTokenStore, prompts: prompts(), stdout: out, stderr: err
    })).toBe(0);
    expect(await runInstanceCli(['instance', 'remove', 'dev'], {
      registry: registryWith([basic]), credentialStore: credentialStore(), keychainTokenStore, fileTokenStore, prompts: prompts({ confirm: true }), stdout: out, stderr: err
    })).toBe(0);
    expect(keychainTokenStore.clearRefreshToken).not.toHaveBeenCalled();
    expect(fileTokenStore.clearRefreshToken).not.toHaveBeenCalled();
  });
});
