/**
 * Tests for the TokenStore contract.
 *
 * A TokenStore persists the OAuth refresh token per account key so a fresh
 * process can refresh without a new browser sign-in. The client depends only
 * on this interface; production uses an OS-keychain-backed store, tests use
 * the in-memory one.
 */

import { jest } from '@jest/globals';
import {
  InMemoryTokenStore,
  KeychainTokenStore,
  FileTokenStore,
  createDefaultTokenStore
} from '../src/token-store.js';
import { promises as fs, constants } from 'node:fs';
import { createHash } from 'node:crypto';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';

const tokenName = account => 'token-' + createHash('sha256').update(account, 'utf8').digest('hex');

describe('InMemoryTokenStore', () => {
  it('returns null for an account with no stored token', async () => {
    const store = new InMemoryTokenStore();
    expect(await store.getRefreshToken('acct')).toBeNull();
  });

  it('round-trips a stored refresh token per account', async () => {
    const store = new InMemoryTokenStore();
    await store.setRefreshToken('caleb@dev', 'rt-1');
    await store.setRefreshToken('caleb@prod', 'rt-2');
    expect(await store.getRefreshToken('caleb@dev')).toBe('rt-1');
    expect(await store.getRefreshToken('caleb@prod')).toBe('rt-2');
  });

  it('clears a stored token', async () => {
    const store = new InMemoryTokenStore();
    await store.setRefreshToken('acct', 'rt-1');
    await store.clearRefreshToken('acct');
    expect(await store.getRefreshToken('acct')).toBeNull();
  });
});

(process.platform === 'win32' ? describe.skip : describe)('FileTokenStore', () => {
  let baseDir;
  const tokenFile = account => join(baseDir, tokenName(account));
  beforeEach(async () => {
    baseDir = await fs.mkdtemp(join(tmpdir(), 'hpm-tokens-'));
    await fs.rm(baseDir, { recursive: true, force: true }); // ensure store creates it
  });
  afterEach(async () => {
    jest.restoreAllMocks();
    await fs.rm(baseDir, { recursive: true, force: true });
  });

  it('returns null for an account with no stored token', async () => {
    const store = new FileTokenStore({ baseDir });
    expect(await store.getRefreshToken('dev')).toBeNull();
  });

  it('round-trips a stored refresh token per account', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'rt-dev');
    await store.setRefreshToken('prod', 'rt-prod');
    expect(await store.getRefreshToken('dev')).toBe('rt-dev');
    expect(await store.getRefreshToken('prod')).toBe('rt-prod');
  });

  it('persists the token file with 0600 perms in a 0700 dir', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'rt-dev');
    const fileMode = (await fs.stat(tokenFile('dev'))).mode & 0o777;
    const dirMode = (await fs.stat(baseDir)).mode & 0o777;
    expect(fileMode).toBe(0o600);
    expect(dirMode).toBe(0o700);
  });

  it('overwrites an existing token', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'rt-1');
    await store.setRefreshToken('dev', 'rt-2');
    expect(await store.getRefreshToken('dev')).toBe('rt-2');
  });

  it('clears a stored token', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'rt-1');
    await store.clearRefreshToken('dev');
    expect(await store.getRefreshToken('dev')).toBeNull();
  });

  it('rejects an account key that could escape the token dir', async () => {
    const store = new FileTokenStore({ baseDir });
    await expect(store.setRefreshToken('../evil', 'x')).rejects.toThrow(/unsafe account/);
    await expect(store.getRefreshToken('a/b')).rejects.toThrow(/unsafe account/);
  });

  it.each(['', '.', '..', 'a\\b', 'C:dev', 'a\n', 'a\0', 'a b', 'a'.repeat(201), null])('rejects invalid account %p on every operation', async account => {
    const store = new FileTokenStore({ baseDir });
    for (const operation of [() => store.getRefreshToken(account), () => store.setRefreshToken(account, 'fake'), () => store.clearRefreshToken(account)]) {
      await expect(operation()).rejects.toThrow(/unsafe account/);
    }
  });

  it.each(['symlink', 'regular file', '0755 directory'])('rejects unsafe base: %s', async kind => {
    if (kind === 'regular file') await fs.writeFile(baseDir, 'fake');
    else if (kind === '0755 directory') await fs.mkdir(baseDir, { mode: 0o755 });
    else await fs.symlink(tmpdir(), baseDir);
    await expect(new FileTokenStore({ baseDir }).setRefreshToken('dev', 'fake')).rejects.toThrow();
  });

  it('propagates directory setup and chmod failures', async () => {
    const failure = Object.assign(new Error('setup denied'), { code: 'EACCES' });
    jest.spyOn(fs, 'mkdir').mockRejectedValueOnce(failure);
    await expect(new FileTokenStore({ baseDir }).setRefreshToken('dev', 'fake')).rejects.toBe(failure);
    jest.restoreAllMocks();
    jest.spyOn(fs, 'chmod').mockRejectedValueOnce(failure);
    await expect(new FileTokenStore({ baseDir }).setRefreshToken('dev', 'fake')).rejects.toBe(failure);
    expect(await fs.readdir(baseDir)).toEqual([]);
  });

  // A concurrent creator wins mkdir: the store must re-validate what now exists.
  const raceMkdir = create => {
    const mkdir = fs.mkdir.bind(fs);
    jest.spyOn(fs, 'mkdir').mockImplementationOnce(async path => {
      await create(path, mkdir);
      throw Object.assign(new Error('EEXIST: file already exists'), { code: 'EEXIST' });
    });
  };

  it('accepts a private token directory created concurrently (EEXIST)', async () => {
    raceMkdir((path, mkdir) => mkdir(path, { mode: 0o700 }));
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'inert-raced-token');
    expect(await store.getRefreshToken('dev')).toBe('inert-raced-token');
  });

  it.each([
    ['an exposed directory', async (path, mkdir) => { await mkdir(path); await fs.chmod(path, 0o755); }],
    ['a symlink', async path => fs.symlink(tmpdir(), path)]
  ])('re-validates a token directory raced into existence as %s (EEXIST)', async (_kind, create) => {
    raceMkdir(create);
    await expect(new FileTokenStore({ baseDir }).setRefreshToken('dev', 'fake')).rejects.toThrow(/unsafe token directory/);
  });

  it('re-validates a missing intermediate directory raced in as a symlink (EEXIST)', async () => {
    const nested = join(baseDir, 'missing', 'happy-platform-mcp');
    await fs.mkdir(baseDir, { mode: 0o700 });
    raceMkdir(path => fs.symlink(tmpdir(), path));
    await expect(new FileTokenStore({ baseDir: nested }).setRefreshToken('dev', 'fake'))
      .rejects.toThrow(/unsafe token directory ancestor .*missing/);
    expect((await fs.lstat(join(baseDir, 'missing'))).isSymbolicLink()).toBe(true);
  });

  it('reads a complete opened token even after atomic replacement unlinks its inode', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'old-inert-token');
    const open = fs.open.bind(fs);
    jest.spyOn(fs, 'open').mockImplementation(async (...args) => {
      const handle = await open(...args);
      if (args[0].endsWith('/' + tokenName('dev'))) {
        await store.setRefreshToken('dev', 'new-inert-token');
        expect((await handle.stat()).nlink).toBe(0);
      }
      return handle;
    });
    expect(await store.getRefreshToken('dev')).toBe('old-inert-token');
    jest.restoreAllMocks();
    expect(await store.getRefreshToken('dev')).toBe('new-inert-token');
  });

  it('rejects multiply linked token files', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'inert-token');
    await fs.link(tokenFile('dev'), join(baseDir, 'alias'));
    await expect(store.getRefreshToken('dev')).rejects.toThrow(/unsafe token file/);
    await expect(store.setRefreshToken('dev', 'other-inert-token')).rejects.toThrow(/unsafe token file/);
  });

  it('tolerates a path lookup that observes the inode just unlinked by a concurrent rename', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'inert-token');
    const lstat = fs.lstat.bind(fs);
    jest.spyOn(fs, 'lstat').mockImplementation(async path => {
      const stat = await lstat(path);
      if (path.endsWith('/' + tokenName('dev'))) stat.nlink = 0;
      return stat;
    });
    expect(await store.getRefreshToken('dev')).toBe('inert-token');
    await store.setRefreshToken('dev', 'other-inert-token');
    jest.restoreAllMocks();
    expect(await store.getRefreshToken('dev')).toBe('other-inert-token');
  });

  it('rejects unsafe existing token files without reading or replacing them', async () => {
    await fs.mkdir(baseDir, { mode: 0o700 });
    const target = tokenFile('dev');
    await fs.symlink('missing', target);
    const store = new FileTokenStore({ baseDir });
    await expect(store.getRefreshToken('dev')).rejects.toThrow();
    await expect(store.setRefreshToken('dev', 'fake')).rejects.toThrow();
    await fs.unlink(target);
    await fs.writeFile(target, 'fake', { mode: 0o644 });
    await expect(store.getRefreshToken('dev')).rejects.toThrow();
    await fs.unlink(target);
    await fs.mkdir(target);
    await expect(store.getRefreshToken('dev')).rejects.toThrow();
  });

  it('propagates non-missing read errors', async () => {
    await fs.mkdir(baseDir, { mode: 0o700 });
    await fs.writeFile(tokenFile('dev'), 'fake', { mode: 0o600 });
    const failure = Object.assign(new Error('read denied'), { code: 'EACCES' });
    jest.spyOn(fs, 'open').mockRejectedValueOnce(failure);
    await expect(new FileTokenStore({ baseDir }).getRefreshToken('dev')).rejects.toBe(failure);
  });

  it('cleans its temporary file on rename failure and preserves the old token', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'old-fake');
    const failure = new Error('rename denied');
    jest.spyOn(fs, 'rename').mockRejectedValueOnce(failure);
    await expect(store.setRefreshToken('dev', 'new-fake')).rejects.toBe(failure);
    expect(await fs.readdir(baseDir)).toEqual([tokenName('dev')]);
    expect(await store.getRefreshToken('dev')).toBe('old-fake');
  });

  it('fails closed on Windows instead of treating chmod as an ACL', async () => {
    const descriptor = Object.getOwnPropertyDescriptor(process, 'platform');
    Object.defineProperty(process, 'platform', { value: 'win32', configurable: true });
    try {
      await expect(new FileTokenStore({ baseDir }).setRefreshToken('dev', 'fake')).rejects.toThrow(/POSIX|Windows/);
      await expect(new FileTokenStore({ baseDir }).getRefreshToken('dev')).rejects.toThrow(/POSIX|Windows/);
      await expect(new FileTokenStore({ baseDir }).clearRefreshToken('dev')).rejects.toThrow(/POSIX|Windows/);
    } finally {
      Object.defineProperty(process, 'platform', descriptor);
    }
  });

  it('publishes only complete tokens with unique exclusive 0600 temporary files', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'seed-fake');
    const tokens = Array.from({ length: 12 }, (_, i) => 'fake-' + i + '-'.repeat(32768));
    const open = jest.spyOn(fs, 'open');
    const observed = [];
    await Promise.all([
      ...tokens.map(token => store.setRefreshToken('dev', token)),
      (async () => {
        for (let i = 0; i < 24; i++) observed.push(await store.getRefreshToken('dev'));
      })()
    ]);
    expect(observed.every(token => token === 'seed-fake' || tokens.includes(token))).toBe(true);
    expect(tokens).toContain(await store.getRefreshToken('dev'));
    const writes = open.mock.calls.filter(([, flags]) => typeof flags === 'number' && (flags & constants.O_EXCL));
    expect(writes).toHaveLength(12);
    expect(new Set(writes.map(([path]) => path)).size).toBe(12);
    expect(writes.every(([, , mode]) => mode === 0o600)).toBe(true);
    expect(await fs.readdir(baseDir)).toEqual([tokenName('dev')]);
  });

  it('cleans a temporary file after partial write failure', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'old-fake');
    const originalOpen = fs.open.bind(fs);
    const failure = new Error('write denied');
    jest.spyOn(fs, 'open').mockImplementation(async (...args) => {
      const handle = await originalOpen(...args);
      if (typeof args[1] === 'number' && (args[1] & constants.O_EXCL)) {
        jest.spyOn(handle, 'writeFile').mockImplementation(async () => {
          await handle.write('partial-fake');
          throw failure;
        });
      }
      return handle;
    });
    await expect(store.setRefreshToken('dev', 'new-fake')).rejects.toBe(failure);
    expect(await fs.readdir(baseDir)).toEqual([tokenName('dev')]);
    expect(await store.getRefreshToken('dev')).toBe('old-fake');
  });

  it('rejects a directory owned by another uid', async () => {
    await fs.mkdir(baseDir, { mode: 0o700 });
    const original = fs.lstat.bind(fs);
    jest.spyOn(fs, 'lstat').mockImplementation(async path => {
      const stat = await original(path);
      if (path === baseDir) stat.uid = process.getuid() + 1;
      return stat;
    });
    await expect(new FileTokenStore({ baseDir }).setRefreshToken('dev', 'fake')).rejects.toThrow(/owner|unsafe/);
  });

  it('smokes the environment-selected file store across fresh instances', async () => {
    const previous = { store: process.env.SERVICENOW_TOKEN_STORE, xdg: process.env.XDG_CONFIG_HOME };
    const configHome = await fs.mkdtemp(join(tmpdir(), 'hpm-selected-'));
    try {
      process.env.SERVICENOW_TOKEN_STORE = 'file';
      process.env.XDG_CONFIG_HOME = configHome;
      const selected = createDefaultTokenStore();
      expect(selected).toBeInstanceOf(FileTokenStore);
      await selected.setRefreshToken('synthetic-user@dev', 'inert-selected-token');
      expect(await createDefaultTokenStore().getRefreshToken('synthetic-user@dev')).toBe('inert-selected-token');
      await createDefaultTokenStore().clearRefreshToken('synthetic-user@dev');
      expect(await selected.getRefreshToken('synthetic-user@dev')).toBeNull();
    } finally {
      if (previous.store === undefined) delete process.env.SERVICENOW_TOKEN_STORE;
      else process.env.SERVICENOW_TOKEN_STORE = previous.store;
      if (previous.xdg === undefined) delete process.env.XDG_CONFIG_HOME;
      else process.env.XDG_CONFIG_HOME = previous.xdg;
      await fs.rm(configHome, { recursive: true, force: true });
    }
  });

  it.each(['identity-v1-' + 'a'.repeat(64), 'user@dev', 'x'.repeat(200)])('accepts safe key %s', async account => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken(account, 'inert-safe-token');
    expect(await store.getRefreshToken(account)).toBe('inert-safe-token');
  });

  it.each(['relative', '/', '/tmp/../escape', '/tmp/control\n'])('rejects unsafe directory path %p', baseDir => {
    expect(() => new FileTokenStore({ baseDir })).toThrow(/unsafe token directory/);
  });

  it('stores accounts differing only by case in distinct lowercase-hex files on the real filesystem', async () => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('u@Dev', 'inert-upper-token');
    expect(await store.getRefreshToken('u@dev')).toBeNull();
    await store.setRefreshToken('u@dev', 'inert-lower-token');
    expect(await store.getRefreshToken('u@Dev')).toBe('inert-upper-token');
    expect(await store.getRefreshToken('u@dev')).toBe('inert-lower-token');
    expect((await fs.readdir(baseDir)).sort()).toEqual([tokenName('u@Dev'), tokenName('u@dev')].sort());
    await store.clearRefreshToken('u@Dev');
    expect(await store.getRefreshToken('u@dev')).toBe('inert-lower-token');
  });

  it.each(['u@Dev', 'u@dev', 'x'.repeat(200), 'identity-v1-' + 'A'.repeat(64)])('names %s with a case-insensitive-safe digest', account => {
    const name = basename(new FileTokenStore({ baseDir })._fileFor(account));
    expect(name).toBe(tokenName(account));
    expect(name).toMatch(/^token-[0-9a-f]{64}$/);
  });

  it('never reads legacy token-<account> names; a new sign-in is required', async () => {
    await fs.mkdir(baseDir, { mode: 0o700 });
    await fs.writeFile(join(baseDir, 'token-dev'), 'inert-legacy-token', { mode: 0o600 });
    const store = new FileTokenStore({ baseDir });
    expect(await store.getRefreshToken('dev')).toBeNull();
    await store.clearRefreshToken('dev');
    expect(await fs.readFile(join(baseDir, 'token-dev'), 'utf8')).toBe('inert-legacy-token');
  });

  it.each([
    ['chmod', 'close'],
    ['write', 'close'],
    ['rename', 'unlink'],
    ['write', 'unlink']
  ])('preserves the primary %s failure when %s cleanup also fails', async (stage, cleanup) => {
    const store = new FileTokenStore({ baseDir });
    await store.setRefreshToken('dev', 'old-fake');
    const primary = new Error(stage + ' denied');
    const cleanupFailure = new Error(cleanup + ' cleanup denied');
    const originalOpen = fs.open.bind(fs);
    jest.spyOn(fs, 'open').mockImplementation(async (...args) => {
      const handle = await originalOpen(...args);
      if (typeof args[1] === 'number' && (args[1] & constants.O_EXCL)) {
        if (stage === 'chmod') jest.spyOn(handle, 'chmod').mockRejectedValueOnce(primary);
        if (stage === 'write') jest.spyOn(handle, 'writeFile').mockRejectedValueOnce(primary);
        if (cleanup === 'close') {
          const close = handle.close.bind(handle);
          jest.spyOn(handle, 'close').mockImplementation(async () => { await close(); throw cleanupFailure; });
        }
      }
      return handle;
    });
    if (stage === 'rename') jest.spyOn(fs, 'rename').mockRejectedValueOnce(primary);
    if (cleanup === 'unlink') jest.spyOn(fs, 'unlink').mockRejectedValueOnce(cleanupFailure);
    await expect(store.setRefreshToken('dev', 'new-fake')).rejects.toBe(primary);
    jest.restoreAllMocks();
    // A failed close still unlinks; only a failed unlink may leave the private temporary.
    if (cleanup === 'close') expect(await fs.readdir(baseDir)).toEqual([tokenName('dev')]);
    expect(await store.getRefreshToken('dev')).toBe('old-fake');
  });

  describe('ancestor validation', () => {
    let root;
    beforeEach(async () => {
      root = await fs.realpath(await fs.mkdtemp(join(tmpdir(), 'hpm-ancestors-')));
    });
    afterEach(async () => {
      await fs.chmod(root, 0o700);
      await fs.rm(root, { recursive: true, force: true });
    });

    it.each(['777', '775', '757'])('rejects a %s writable ancestor by path before creating anything', async mode => {
      const exposed = join(root, 'exposed');
      await fs.mkdir(exposed);
      await fs.chmod(exposed, parseInt(mode, 8));
      const store = new FileTokenStore({ baseDir: join(exposed, 'config', 'happy-platform-mcp') });
      const error = await store.setRefreshToken('dev', 'inert-secret-token').catch(e => e);
      expect(error.message).toContain('unsafe token directory ancestor ' + exposed);
      expect(error.message).toMatch(/owned by the current user or root/);
      expect(error.message).toMatch(/chmod go-w/);
      expect(error.message).not.toContain('inert-secret-token');
      expect(await fs.readdir(exposed)).toEqual([]);
    });

    it('accepts a sticky writable ancestor such as /tmp', async () => {
      const sticky = join(root, 'sticky');
      await fs.mkdir(sticky);
      await fs.chmod(sticky, 0o1777);
      const store = new FileTokenStore({ baseDir: join(sticky, 'happy-platform-mcp') });
      await store.setRefreshToken('dev', 'inert-sticky-token');
      expect(await store.getRefreshToken('dev')).toBe('inert-sticky-token');
    });

    it('rejects an ancestor owned by another uid by path', async () => {
      const foreign = join(root, 'foreign');
      await fs.mkdir(foreign, { mode: 0o755 });
      const lstat = fs.lstat.bind(fs);
      jest.spyOn(fs, 'lstat').mockImplementation(async path => {
        const stat = await lstat(path);
        if (path === foreign) stat.uid = process.getuid() + 1;
        return stat;
      });
      await expect(new FileTokenStore({ baseDir: join(foreign, 'happy-platform-mcp') }).setRefreshToken('dev', 'fake'))
        .rejects.toThrow('unsafe token directory ancestor ' + foreign);
      jest.restoreAllMocks();
      expect(await fs.readdir(foreign)).toEqual([]);
    });

    it('rejects a non-directory ancestor by path', async () => {
      const file = join(root, 'file');
      await fs.writeFile(file, 'inert');
      await expect(new FileTokenStore({ baseDir: join(file, 'x', 'happy-platform-mcp') }).setRefreshToken('dev', 'fake'))
        .rejects.toThrow('unsafe token directory ancestor ' + file);
    });

    it('creates missing intermediate directories privately after validating existing ancestors', async () => {
      const store = new FileTokenStore({ baseDir: join(root, 'a', 'b', 'happy-platform-mcp') });
      await store.setRefreshToken('dev', 'inert-nested-token');
      expect((await fs.stat(join(root, 'a'))).mode & 0o022).toBe(0);
      expect((await fs.stat(join(root, 'a', 'b'))).mode & 0o022).toBe(0);
      expect(await store.getRefreshToken('dev')).toBe('inert-nested-token');
    });

    it('follows a symlinked ancestor and stores in its validated private target', async () => {
      const target = join(root, 'target');
      await fs.mkdir(target, { mode: 0o700 });
      await fs.symlink(target, join(root, 'link'));
      const store = new FileTokenStore({ baseDir: join(root, 'link', 'happy-platform-mcp') });
      await store.setRefreshToken('dev', 'inert-linked-token');
      expect(await fs.readdir(join(target, 'happy-platform-mcp'))).toEqual([tokenName('dev')]);
      expect(await store.getRefreshToken('dev')).toBe('inert-linked-token');
    });

    it('rejects a symlinked ancestor whose target chain is writable, naming the resolved path', async () => {
      const exposed = join(root, 'exposed');
      await fs.mkdir(exposed);
      await fs.chmod(exposed, 0o777);
      await fs.symlink(exposed, join(root, 'link'));
      await expect(new FileTokenStore({ baseDir: join(root, 'link', 'happy-platform-mcp') }).setRefreshToken('dev', 'fake'))
        .rejects.toThrow('unsafe token directory ancestor ' + exposed);
      expect(await fs.readdir(exposed)).toEqual([]);
    });

    it('rejects a dangling symlink where an intermediate directory is needed', async () => {
      await fs.symlink(join(root, 'nowhere'), join(root, 'dangling'));
      await expect(new FileTokenStore({ baseDir: join(root, 'dangling', 'happy-platform-mcp') }).setRefreshToken('dev', 'fake'))
        .rejects.toThrow(/unsafe token directory ancestor .*dangling/);
      await expect(fs.lstat(join(root, 'nowhere'))).rejects.toMatchObject({ code: 'ENOENT' });
    });
  });
});

describe('FileTokenStore Windows privacy boundary', () => {
  it('rejects file operations before touching storage when POSIX privacy is unavailable', async () => {
    const descriptor = Object.getOwnPropertyDescriptor(process, 'platform');
    const store = new FileTokenStore({ baseDir: join(tmpdir(), 'inert-windows-store') });
    Object.defineProperty(process, 'platform', { value: 'win32', configurable: true });
    try {
      await expect(store.getRefreshToken('dev')).rejects.toThrow(/POSIX|Windows/);
      await expect(store.setRefreshToken('dev', 'fake')).rejects.toThrow(/POSIX|Windows/);
      await expect(store.clearRefreshToken('dev')).rejects.toThrow(/POSIX|Windows/);
      expect(createDefaultTokenStore('keychain')).toBeInstanceOf(KeychainTokenStore);
    } finally {
      Object.defineProperty(process, 'platform', descriptor);
    }
  });
});


describe('createDefaultTokenStore', () => {
  it('selects the keychain when unset or "keychain"', () => {
    expect(createDefaultTokenStore(undefined)).toBeInstanceOf(KeychainTokenStore);
    expect(createDefaultTokenStore('')).toBeInstanceOf(KeychainTokenStore);
    expect(createDefaultTokenStore('keychain')).toBeInstanceOf(KeychainTokenStore);
  });

  it('selects the file store for "file"', () => {
    expect(createDefaultTokenStore('file')).toBeInstanceOf(FileTokenStore);
  });

  it('rejects an unknown value instead of guessing', () => {
    expect(() => createDefaultTokenStore('vault')).toThrow(/SERVICENOW_TOKEN_STORE/);
  });
});

describe('KeychainTokenStore (with injected entry factory)', () => {
  it('returns null when the keychain has no entry (getPassword returns null)', async () => {
    const store = new KeychainTokenStore({
      createEntry: () => ({ getPassword: () => null })
    });
    expect(await store.getRefreshToken('acct')).toBeNull();
  });

  it('fails loud (rethrows) when the keychain itself errors, instead of masking it as "no token"', async () => {
    const store = new KeychainTokenStore({
      createEntry: () => ({ getPassword: () => { throw new Error('keychain locked'); } })
    });
    await expect(store.getRefreshToken('acct')).rejects.toThrow(/keychain locked/);
  });
  it('waits for an async keychain write before resolving', async () => {
    let release;
    const writePending = new Promise(resolve => { release = resolve; });
    let writes = 0;
    const store = new KeychainTokenStore({
      createEntry: () => ({
        setPassword: async () => {
          writes++;
          await writePending;
          return 'stored';
        }
      })
    });

    const write = store.setRefreshToken('acct', 'refresh-secret');
    await Promise.resolve();
    expect(writes).toBe(1);
    expect(await Promise.race([write.then(() => 'settled'), Promise.resolve('pending')])).toBe('pending');
    release();
    expect(await write).toBe('stored');
  });

  it('propagates an async keychain write rejection', async () => {
    const backendError = new Error('keychain write failed refresh-secret');
    const store = new KeychainTokenStore({
      createEntry: () => ({
        setPassword: async () => { throw backendError; }
      })
    });

    await expect(store.setRefreshToken('acct', 'refresh-secret')).rejects.toBe(backendError);
  });

  it('does not log raw keychain backend details or account identifiers on read failure', async () => {
    const consoleError = jest.spyOn(console, 'error').mockImplementation(() => {});
    const store = new KeychainTokenStore({
      createEntry: () => ({ getPassword: async () => { throw new Error('backend-secret-detail'); } })
    });

    await expect(store.getRefreshToken('sensitive-account')).rejects.toThrow();
    expect(consoleError).toHaveBeenCalledWith('Keychain read failed');
    expect(consoleError.mock.calls.flat().join(' ')).not.toContain('backend-secret-detail');
    expect(consoleError.mock.calls.flat().join(' ')).not.toContain('sensitive-account');
    consoleError.mockRestore();
  });

  it('fails loud when clearing the keychain throws', async () => {
    const backendError = new Error('keychain locked');
    const store = new KeychainTokenStore({
      createEntry: () => ({ deletePassword: () => { throw backendError; } })
    });
    await expect(store.clearRefreshToken('acct')).rejects.toBe(backendError);
  });

  it('treats a false delete result as an idempotent missing entry', async () => {
    const store = new KeychainTokenStore({
      createEntry: () => ({ deletePassword: () => false })
    });
    await expect(store.clearRefreshToken('acct')).resolves.toBeUndefined();
  });
});
