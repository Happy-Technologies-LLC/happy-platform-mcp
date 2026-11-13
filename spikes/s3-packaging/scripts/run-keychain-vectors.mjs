#!/usr/bin/env node
// Orchestrates the JS<->Rust keychain golden vectors on the current OS.
//   node scripts/run-keychain-vectors.mjs
// Only touches service "happy-platform-mcp" with test-only accounts containing "__s3_spike__"
// (or the throw-away instance name "s3spike"). Never reads real credentials. Always cleans up.
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { InstanceCredentialStore } from '../../../src/instance-credential-store.js';
import { KeychainTokenStore } from '../../../src/token-store.js';
import { oauthRefreshTokenAccount } from '../../../src/servicenow-client.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.join(here, '..');
const SERVICE = 'happy-platform-mcp';
const osUser = os.userInfo().username;
const log = (m) => console.log(`[s3] ${m}`);

const refreshInstance = { url: 'https://__s3_spike__.invalid/', clientId: 's3-spike-client' };
const refreshAccount = oauthRefreshTokenAccount(refreshInstance, { osUser });

const vectors = [
  { direction: 'js_to_rust', via: 'entry', account: 'keychain:instance/__s3_spike__/password',
    value: 'S3-SPIKE-ascii-password-not-a-secret' },
  { direction: 'js_to_rust', via: 'entry', account: 'keychain:instance/__s3_spike__/client-secret',
    value: 'S3-SPIKE ünï ☃ 🚀 "q" \\b\nline2\ttab' },
  { direction: 'js_to_rust', via: 'instance-store-public-api', account: 'keychain:instance/s3spike/password',
    value: 'S3-SPIKE-public-api-value' },
  { direction: 'js_to_rust', via: 'token-store', account: refreshAccount, value: 'S3-SPIKE-refresh-token-not-real',
    identityInput: { ...refreshInstance, osUser } },
  { direction: 'rust_to_js', via: 'entry', account: 'keychain:instance/__s3_spike__/rust-to-js',
    value: 'S3-SPIKE-written-by-rust ✓' }
];

const credStore = new InstanceCredentialStore();
const tokenStore = new KeychainTokenStore();
const vectorsFile = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 's3-vectors-')), 'vectors.json');
fs.writeFileSync(vectorsFile, JSON.stringify({ service: SERVICE, vectors }), { mode: 0o600 });

function rust(testName) {
  log(`cargo test ${testName}`);
  const r = spawnSync('cargo', ['test', '--test', 'keychain_vectors', testName, '--', '--ignored', '--nocapture', '--test-threads=1'],
    { cwd: root, env: { ...process.env, HPM_S3_VECTORS: vectorsFile }, encoding: 'utf8', timeout: 300_000 });
  process.stdout.write((r.stdout || '').split('\n').filter((l) => /^(OK|test )/.test(l)).join('\n') + '\n');
  if (r.status !== 0) { process.stdout.write(r.stderr || ''); throw new Error(`rust step ${testName} failed (status ${r.status})`); }
}

async function cleanup() {
  log('cleanup: deleting test items');
  for (const v of vectors) {
    let before; let deleted = 'n/a';
    try {
      if (v.via === 'instance-store-public-api') {
        deleted = JSON.stringify(await credStore.deleteSecret(v.account));
      } else if (v.via === 'token-store') {
        await tokenStore.clearRefreshToken(v.account); deleted = 'cleared';
      } else {
        const e = await credStore._entry(v.account);
        before = await e.getPassword();
        deleted = before == null ? 'already-absent' : String(await e.deletePassword());
      }
    } catch (err) { deleted = `error:${err.message}`; }
    const after = await (await credStore._entry(v.account)).getPassword();
    log(`  delete ${v.account.slice(0, 60)} -> ${deleted}; present-after=${after != null}`);
    if (after != null) process.exitCode = 1;
  }
}

try {
  // Pre-clean any stale leftovers from an aborted run.
  for (const v of vectors) { try { await (await credStore._entry(v.account)).deletePassword(); } catch {} }

  log(`write items from JS (service=${SERVICE}, os=${process.platform}, osUser=${osUser})`);
  for (const v of vectors.filter((x) => x.direction === 'js_to_rust')) {
    if (v.via === 'entry') await (await credStore._entry(v.account)).setPassword(v.value);
    else if (v.via === 'instance-store-public-api') await credStore.setSecret(v.account, v.value);
    else if (v.via === 'token-store') await tokenStore.setRefreshToken(v.account, v.value);
    log(`  wrote ${v.account.slice(0, 60)} via ${v.via}`);
  }
  rust('js_written_items_are_readable_from_rust');
  rust('rust_writes_items_for_js');
  for (const v of vectors.filter((x) => x.direction === 'rust_to_js')) {
    const got = await (await credStore._entry(v.account)).getPassword();
    const ok = got === v.value;
    log(`  JS read of Rust-written ${v.account}: ${ok ? 'MATCH' : 'MISMATCH'}`);
    if (!ok) process.exitCode = 1;
  }
} catch (err) {
  console.error(`[s3] FAILED: ${err.message}`);
  process.exitCode = 1;
} finally {
  await cleanup();
  try { rust('all_vector_items_are_absent'); } catch (err) { console.error(`[s3] ${err.message}`); process.exitCode = 1; }
  fs.rmSync(path.dirname(vectorsFile), { recursive: true, force: true });
  log(process.exitCode ? 'RESULT: FAIL' : 'RESULT: PASS');
}
