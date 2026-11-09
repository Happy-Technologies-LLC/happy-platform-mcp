/**
 * Public links printed by the CLI and servers. One place to change them.
 */

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { PACKAGE_VERSION } from './package-info.js';

export const UPDATES_URL = 'https://happy-tech.biz/mcp';
export const DISCUSSIONS_URL = 'https://github.com/Happy-Technologies-LLC/happy-platform-mcp/discussions';
/** Where opt-in usage pings go. Override with HAPPY_MCP_TELEMETRY_URL. */
export const TELEMETRY_URL = 'https://build01-lab.tail0490c9.ts.net:10000/api/mcp/telemetry';

/**
 * One line shown after a successful setup step. Kept short so it reads as a
 * pointer, not a banner. Goes to stderr so stdout stays machine-readable.
 */
export function updatesHint() {
  return `Release notes and security advisories: ${UPDATES_URL}`;
}

/** File that remembers the last version that printed the hint. */
export function lastSeenVersionPath({ env = process.env, homeDir = os.homedir() } = {}) {
  const base = env.XDG_CONFIG_HOME?.trim() || path.join(homeDir, '.config');
  return path.join(base, 'happy-platform-mcp', 'last-seen-version');
}

/**
 * The hint for server start: returned only on first run or after the
 * installed version changes, then remembered so the next start stays quiet.
 * Returns null when there is nothing to say. Never throws: a read-only home
 * directory just means the hint may repeat.
 */
export function versionChangeHint({
  env = process.env,
  homeDir = os.homedir(),
  version = PACKAGE_VERSION,
  fsImpl = fs
} = {}) {
  if (['1', 'true', 'yes', 'on'].includes(String(env.HAPPY_MCP_QUIET || '').trim().toLowerCase())) return null;
  const file = lastSeenVersionPath({ env, homeDir });
  let previous = null;
  try {
    previous = fsImpl.readFileSync(file, 'utf8').trim() || null;
  } catch {
    previous = null;
  }
  if (previous === version) return null;
  try {
    fsImpl.mkdirSync(path.dirname(file), { recursive: true });
    fsImpl.writeFileSync(file, `${version}\n`);
  } catch {
    // Best effort: an unwritable config dir must never block startup.
  }
  return previous
    ? `Happy MCP updated ${previous} -> ${version}. What changed and security advisories: ${UPDATES_URL}`
    : updatesHint();
}
