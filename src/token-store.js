/**
 * TokenStore — persists the OAuth refresh token per account key.
 *
 * The client depends only on the async interface:
 *   getRefreshToken(account)   -> Promise<string|null>
 *   setRefreshToken(account, t)-> Promise<void>
 *   clearRefreshToken(account) -> Promise<void>
 *
 * `account` is a stable per-identity key (e.g. "<username>@<instanceName>").
 * Production defaults to the OS keychain (KeychainTokenStore). Set
 * SERVICENOW_TOKEN_STORE=file to use FileTokenStore instead; tests inject
 * InMemoryTokenStore.
 */

import { promises as fs, constants } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import { homedir } from 'node:os';
import { join, resolve, dirname, basename, isAbsolute } from 'node:path';

const SERVICE_NAME = 'happy-platform-mcp';

/** In-memory store — no persistence across processes. Used by tests and as a fallback. */
export class InMemoryTokenStore {
  constructor() {
    this._tokens = new Map();
  }

  async getRefreshToken(account) {
    return this._tokens.has(account) ? this._tokens.get(account) : null;
  }

  async setRefreshToken(account, token) {
    this._tokens.set(account, token);
  }

  async clearRefreshToken(account) {
    this._tokens.delete(account);
  }
}

/** Default token directory: $XDG_CONFIG_HOME/happy-platform-mcp, else ~/.config/happy-platform-mcp. */
function defaultTokenDir() {
  const base = process.env.XDG_CONFIG_HOME || join(homedir(), '.config');
  return join(base, 'happy-platform-mcp');
}

/**
 * Optional plaintext POSIX store. Private permissions isolate other OS users,
 * not other processes of this user. Atomic replacement is not refresh locking.
 * Windows is unsupported here: chmod does not establish a private Windows ACL.
 */
export class FileTokenStore {
  constructor({ baseDir = defaultTokenDir() } = {}) {
    if (typeof baseDir !== 'string' || !isAbsolute(baseDir) || /[\x00-\x1f\x7f]/.test(baseDir) ||
        baseDir.split('/').some(part => part === '.' || part === '..') || resolve(baseDir) === '/') {
      throw new Error('unsafe token directory path');
    }
    this._baseDir = resolve(baseDir);
  }

  // Hash the key so case-insensitive filesystems (default macOS APFS) cannot
  // alias keys that differ only by case, such as "u@Dev" and "u@dev".
  _fileFor(account, directory = this._baseDir) {
    if (typeof account !== 'string' || !/^[A-Za-z0-9_@.-]{1,200}$/.test(account) ||
        account === '.' || account.includes('..')) {
      throw new Error('unsafe account key');
    }
    return join(directory, 'token-' + createHash('sha256').update(account, 'utf8').digest('hex'));
  }

  // A concurrent atomic replacement can unlink the inode between path lookup
  // and stat (nlink 0); only extra hard links (nlink > 1) are an alias risk.
  _assertPrivate(stat, path, directory) {
    if ((directory ? !stat.isDirectory() : !stat.isFile()) || stat.isSymbolicLink() ||
        stat.uid !== process.getuid() || (stat.mode & 0o7777) !== (directory ? 0o700 : 0o600) ||
        (!directory && stat.nlink > 1)) {
      throw new Error(directory
        ? `unsafe token directory ${path}: must be a real directory (not a symlink) owned by the current user with mode 0700`
        : `unsafe token file ${path}: must be a regular, singly linked file owned by the current user with mode 0600`);
    }
  }

  // Ancestors must be directories owned by root or this user and not
  // group/world-writable, except sticky directories such as /tmp.
  _assertTrustedAncestor(stat, path) {
    if (!stat.isDirectory() || (stat.uid !== 0 && stat.uid !== process.getuid()) ||
        ((stat.mode & 0o022) && !(stat.mode & 0o1000))) {
      throw new Error(`unsafe token directory ancestor ${path}: must be a real directory owned by the current user or root ` +
        `and not group/world-writable unless sticky (for example, chmod go-w ${path})`);
    }
  }

  /**
   * Validate the canonical parent chain before creating anything, then create
   * missing intermediates one at a time, re-checking each (including EEXIST).
   * Symlinked ancestors are followed; their resolved chain must be trusted.
   */
  async _trustedParent(create) {
    const missing = [];
    let path = dirname(this._baseDir);
    let canonical;
    for (;;) {
      try {
        canonical = await fs.realpath(path);
        break;
      } catch (error) {
        if (!create || (error.code !== 'ENOENT' && error.code !== 'ENOTDIR')) throw error;
        missing.unshift(basename(path));
        path = dirname(path);
      }
    }
    for (let ancestor = canonical; ; ancestor = dirname(ancestor)) {
      this._assertTrustedAncestor(await fs.lstat(ancestor), ancestor);
      if (dirname(ancestor) === ancestor) break;
    }
    for (const name of missing) {
      canonical = join(canonical, name);
      try {
        await fs.mkdir(canonical, { mode: 0o700 });
      } catch (error) {
        if (error.code !== 'EEXIST') throw error;
      }
      this._assertTrustedAncestor(await fs.lstat(canonical), canonical);
    }
    return canonical;
  }

  async _directory(create) {
    if (process.platform === 'win32' || typeof process.getuid !== 'function' || !constants.O_NOFOLLOW) {
      throw new Error('File token storage requires POSIX ownership and permissions; use the keychain on Windows');
    }
    // Check existing objects before chmod: never repair a foreign or exposed dir.
    let stat = null;
    try {
      stat = await fs.lstat(this._baseDir);
    } catch (error) {
      // ENOTDIR means a file sits where an ancestor belongs: report that path.
      if (error.code !== 'ENOENT' && !(create && error.code === 'ENOTDIR')) throw error;
      if (!create) return null;
    }
    if (stat) this._assertPrivate(stat, this._baseDir, true);
    // Resolve system aliases such as macOS /var before operating on canonical paths.
    const directory = join(await this._trustedParent(create && !stat), basename(this._baseDir));
    if (!stat) {
      let created = false;
      try {
        await fs.mkdir(directory, { mode: 0o700 });
        created = true;
      } catch (error) {
        if (error.code !== 'EEXIST') throw error;
      }
      if (created) await fs.chmod(directory, 0o700);
    }
    this._assertPrivate(await fs.lstat(directory), directory, true);
    return directory;
  }

  async _existing(file) {
    try {
      const stat = await fs.lstat(file);
      this._assertPrivate(stat, file, false);
      return stat;
    } catch (error) {
      if (error.code === 'ENOENT') return null;
      throw error;
    }
  }

  async getRefreshToken(account) {
    this._fileFor(account);
    const directory = await this._directory(false);
    if (!directory) return null;
    const file = this._fileFor(account, directory);
    if (!await this._existing(file)) return null;
    let handle;
    try {
      handle = await fs.open(file, constants.O_RDONLY | constants.O_NOFOLLOW);
      this._assertPrivate(await handle.stat(), file, false);
      const token = (await handle.readFile('utf8')).trim();
      return token || null;
    } catch (error) {
      if (error.code === 'ENOENT') return null;
      throw error;
    } finally {
      if (handle) await handle.close();
    }
  }

  async setRefreshToken(account, token) {
    this._fileFor(account);
    const directory = await this._directory(true);
    const file = this._fileFor(account, directory);
    await this._existing(file);
    const temporary = join(directory, '.token-' + randomUUID() + '.tmp');
    let handle;
    let created = false;
    try {
      handle = await fs.open(temporary, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
      created = true;
      await handle.chmod(0o600);
      this._assertPrivate(await handle.stat(), temporary, false);
      await handle.writeFile(token, 'utf8');
      const closing = handle;
      handle = null;
      await closing.close();
      this._assertPrivate(await fs.lstat(directory), directory, true);
      await this._existing(file);
      await fs.rename(temporary, file);
    } catch (error) {
      // Best-effort cleanup must not mask the write/rename failure. Close before
      // unlink (including platforms that forbid deleting open files).
      if (handle) await handle.close().catch(() => {});
      if (created) await fs.unlink(temporary).catch(() => {});
      throw error;
    }
  }

  async clearRefreshToken(account) {
    this._fileFor(account);
    const directory = await this._directory(false);
    if (!directory) return;
    const file = this._fileFor(account, directory);
    if (!await this._existing(file)) return;
    try {
      await fs.unlink(file);
    } catch (error) {
      if (error.code !== 'ENOENT') throw error;
    }
  }
}

/**
 * OS-keychain-backed store (macOS Keychain / libsecret / Windows Credential
 * Manager) via @napi-rs/keyring. Lazily imported so environments without the
 * native module (or that inject a different store) never load it.
 */
export class KeychainTokenStore {
  constructor({ service = SERVICE_NAME, createEntry } = {}) {
    this.service = service;
    this._createEntry = createEntry || null;
    this._Entry = null;
  }

  async _entry(account) {
    if (this._createEntry) {
      return this._createEntry(this.service, account);
    }
    if (!this._Entry) {
      ({ Entry: this._Entry } = await import('@napi-rs/keyring'));
    }
    return new this._Entry(this.service, account);
  }

  async getRefreshToken(account) {
    // A missing entry returns null (no throw). A real fault — missing native
    // module, locked keychain, permission denied — must FAIL LOUD rather than
    // masquerade as "no token" and trigger a silent re-auth.
    try {
      return await (await this._entry(account)).getPassword() ?? null;
    } catch (err) {
      console.error('Keychain read failed');
      throw err;
    }
  }

  async setRefreshToken(account, token) {
    return await (await this._entry(account)).setPassword(token);
  }

  async clearRefreshToken(account) {
    await (await this._entry(account)).deletePassword();
  }
}

/**
 * Build the production token store named by SERVICENOW_TOKEN_STORE.
 * Unset or "keychain" selects the OS keychain; "file" selects FileTokenStore.
 */
export function createDefaultTokenStore(kind = process.env.SERVICENOW_TOKEN_STORE) {
  if (kind === undefined || kind === '' || kind === 'keychain') return new KeychainTokenStore();
  if (kind === 'file') return new FileTokenStore();
  throw new Error(`SERVICENOW_TOKEN_STORE must be "keychain" or "file", got ${JSON.stringify(kind)}`);
}
