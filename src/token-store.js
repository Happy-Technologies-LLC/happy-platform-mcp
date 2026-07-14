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

import { promises as fs } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

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
 * File-backed store — one 0600 file per account (token-<account>) under a 0700
 * dir. Opt in with SERVICENOW_TOKEN_STORE=file when another process must read
 * the same refresh token: macOS gates a keychain item by the code signature of
 * the binary that created it, so a second binary (or a re-signed node) is
 * prompted on every read. A 0600 file is readable by any process running as the
 * user, which is the same exposure as a keychain item opened to all apps.
 */
export class FileTokenStore {
  constructor({ baseDir } = {}) {
    this._baseDir = baseDir || defaultTokenDir();
  }

  _fileFor(account) {
    if (typeof account !== 'string' || account.includes('/') || account.includes('..')) {
      throw new Error(`unsafe account key: ${JSON.stringify(account)}`);
    }
    return join(this._baseDir, `token-${account}`);
  }

  async getRefreshToken(account) {
    let raw;
    try {
      raw = await fs.readFile(this._fileFor(account), 'utf8');
    } catch (err) {
      if (err.code === 'ENOENT') return null;   // no token yet
      throw err;                                 // real fault → FAIL LOUD
    }
    const token = raw.trim();
    return token || null;
  }

  async setRefreshToken(account, token) {
    const file = this._fileFor(account);
    await fs.mkdir(this._baseDir, { recursive: true });
    await fs.chmod(this._baseDir, 0o700).catch(() => {});
    // Write to a temp file then rename so a concurrent reader never sees a
    // half-written token.
    const tmp = `${file}.tmp`;
    await fs.writeFile(tmp, token, { mode: 0o600 });
    await fs.chmod(tmp, 0o600);                   // defeat umask
    await fs.rename(tmp, file);
  }

  async clearRefreshToken(account) {
    try {
      await fs.unlink(this._fileFor(account));
    } catch (err) {
      if (err.code !== 'ENOENT') throw err;
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
