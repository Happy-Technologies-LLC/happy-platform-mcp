// Keychain access for the side-by-side harness. Uses @napi-rs/keyring from the
// 6.2.0 install (the same library and service name 6.2.0 itself uses), so no
// `security` CLI (and no keychain GUI prompt for a new binary) is involved.
// The password is returned to the caller and must never be logged.
import { createRequire } from 'node:module';
import path from 'node:path';

export const SERVICE = 'happy-platform-mcp';
export const PDI_ACCOUNT = 'keychain:instance/dev451274/password';

function keyring(jsPrefix) {
  return createRequire(path.join(path.resolve(jsPrefix), 'package.json'))('@napi-rs/keyring');
}

export function loadPassword(jsPrefix) {
  const { Entry } = keyring(jsPrefix);
  const pw = new Entry(SERVICE, PDI_ACCOUNT).getPassword();
  if (!pw || pw.length < 8) throw new Error('no usable dev451274 password in the keychain');
  return pw;
}

/** Account names only (never the secrets), sorted. */
export function keychainAccounts(jsPrefix) {
  const { findCredentials } = keyring(jsPrefix);
  return findCredentials(SERVICE).map((c) => c.account).sort();
}
