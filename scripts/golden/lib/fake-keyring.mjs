// In-memory/file-backed stand-in for @napi-rs/keyring. Installed by preload.mjs
// so no golden-capture process can ever touch the real OS keychain.
import fs from 'node:fs';

const file = process.env.GOLDEN_KEYRING_FILE || null;

function load() {
  if (!file) return globalThis.__goldenKeyring ??= {};
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch { return {}; }
}
function save(state) {
  if (file) fs.writeFileSync(file, JSON.stringify(state), { mode: 0o600 });
}

export class Entry {
  constructor(service, account) {
    this.key = `${service}\u0000${account}`;
  }
  setPassword(value) {
    const state = load();
    state[this.key] = String(value);
    save(state);
  }
  getPassword() {
    const state = load();
    return Object.hasOwn(state, this.key) ? state[this.key] : null;
  }
  deletePassword() {
    const state = load();
    const had = Object.hasOwn(state, this.key);
    delete state[this.key];
    save(state);
    return had;
  }
}
