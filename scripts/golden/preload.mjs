// Safety preload for every process the golden harness starts (`node --import`).
//  1. @napi-rs/keyring -> fake (real keychain is unreachable).
//  2. Outbound TCP restricted to loopback (no live ServiceNow, no GitHub).
//  3. fetch() to the pinned docs origin is redirected to GOLDEN_RAW_GITHUB_BASE
//     (same approach as tests/helpers/docs-fake-github.js).
import { register } from 'node:module';
import net from 'node:net';

register('./lib/keyring-hook.mjs', import.meta.url);

const LOOPBACK = new Set(['127.0.0.1', 'localhost', '::1', '[::1]']);
const origConnect = net.Socket.prototype.connect;
net.Socket.prototype.connect = function guardedConnect(...args) {
  const first = args[0];
  let host;
  if (first && typeof first === 'object' && !Array.isArray(first)) {
    if (first.path) return origConnect.apply(this, args);
    host = first.host ?? 'localhost';
  } else if (typeof first === 'string' && Number.isNaN(Number(first))) {
    return origConnect.apply(this, args); // unix socket path
  } else {
    host = typeof args[1] === 'string' ? args[1] : 'localhost';
  }
  if (!LOOPBACK.has(String(host).toLowerCase())) {
    const error = new Error(`golden harness blocked non-loopback connection to ${host}`);
    error.code = 'EGOLDENBLOCKED';
    queueMicrotask(() => this.destroy(error));
    return this;
  }
  return origConnect.apply(this, args);
};

const rawBase = process.env.GOLDEN_RAW_GITHUB_BASE;
const realFetch = globalThis.fetch;
const PINNED = 'https://raw.githubusercontent.com';
globalThis.fetch = function goldenFetch(url, init) {
  const text = String(url instanceof Request ? url.url : url);
  if (rawBase && text.startsWith(`${PINNED}/`)) {
    return realFetch(rawBase + text.slice(PINNED.length), init);
  }
  return realFetch(url, init);
};
