/**
 * Opt-in, anonymous usage telemetry.
 *
 * Off unless HAPPY_MCP_TELEMETRY is set to on/1/true/yes. When on, the server
 * counts how many times each tool name is called and posts those counts, plus
 * the package version, transport, Node major version and OS platform, to
 * HAPPY_MCP_TELEMETRY_URL (default: TELEMETRY_URL in project-links.js).
 *
 * Nothing about the ServiceNow side is ever included: no instance URL or
 * name, no usernames, no tool arguments, no results, no hostname. The full
 * payload shape is `buildPing()`; the README documents it verbatim.
 *
 * Sends are best effort: a short timeout, every error swallowed, never a
 * retry. Telemetry must never slow down or break a tool call.
 */

import os from 'node:os';
import { randomUUID } from 'node:crypto';
import { PACKAGE_NAME, PACKAGE_VERSION } from './package-info.js';
import { TELEMETRY_URL } from './project-links.js';

const ON_VALUES = new Set(['1', 'true', 'on', 'yes']);
const FLUSH_INTERVAL_MS = 10 * 60 * 1000;
const FLUSH_THRESHOLD = 200;
const SEND_TIMEOUT_MS = 3000;

export function telemetryEnabled(env = process.env) {
  return ON_VALUES.has(String(env.HAPPY_MCP_TELEMETRY || '').trim().toLowerCase());
}

export function telemetryUrl(env = process.env) {
  const override = String(env.HAPPY_MCP_TELEMETRY_URL || '').trim();
  return override || TELEMETRY_URL;
}

function nodeMajor(version = process.version) {
  const match = /^v?(\d+)/.exec(String(version));
  return match ? `v${match[1]}` : 'unknown';
}

export function createUsageTracker({
  env = process.env,
  fetchImpl = globalThis.fetch,
  schedule = globalThis.setInterval,
  unschedule = globalThis.clearInterval,
  proc = process,
  now = () => new Date(),
  platform = os.platform(),
  nodeVersion = process.version,
  sessionId = randomUUID()
} = {}) {
  const enabled = telemetryEnabled(env);
  const url = telemetryUrl(env);
  const counts = new Map();
  let transport = 'unknown';
  let pending = 0;
  let timer = null;
  let flushing = false;

  function buildPing() {
    const tools = {};
    for (const [name, count] of [...counts.entries()].sort()) tools[name] = count;
    return {
      v: 1,
      package: PACKAGE_NAME,
      version: PACKAGE_VERSION,
      session: sessionId,
      transport,
      node: nodeMajor(nodeVersion),
      platform,
      tools,
      sent_at: now().toISOString()
    };
  }

  async function flush() {
    if (!enabled || flushing || counts.size === 0) return false;
    flushing = true;
    const body = JSON.stringify(buildPing());
    counts.clear();
    pending = 0;
    try {
      if (typeof fetchImpl !== 'function') return false;
      const controller = typeof AbortController === 'function' ? new AbortController() : null;
      const abortTimer = controller ? setTimeout(() => controller.abort(), SEND_TIMEOUT_MS) : null;
      if (abortTimer && typeof abortTimer.unref === 'function') abortTimer.unref();
      try {
        await fetchImpl(url, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body,
          signal: controller ? controller.signal : undefined
        });
      } finally {
        if (abortTimer) clearTimeout(abortTimer);
      }
      return true;
    } catch {
      // Best effort by design. Never surface, never retry.
      return false;
    } finally {
      flushing = false;
    }
  }

  function start() {
    if (!enabled || timer) return;
    timer = schedule(() => { flush(); }, FLUSH_INTERVAL_MS);
    if (timer && typeof timer.unref === 'function') timer.unref();
    if (proc && typeof proc.once === 'function') {
      proc.once('beforeExit', () => { flush(); });
    }
  }

  function stop() {
    if (timer) {
      unschedule(timer);
      timer = null;
    }
  }

  function record(toolName) {
    if (!enabled) return;
    const name = typeof toolName === 'string' && toolName ? toolName : 'unknown';
    counts.set(name, (counts.get(name) || 0) + 1);
    pending += 1;
    if (!timer) start();
    if (pending >= FLUSH_THRESHOLD) flush();
  }

  function setTransport(value) {
    if (typeof value === 'string' && value) transport = value;
  }

  function describe() {
    return enabled
      ? `Telemetry: on (anonymous tool-call counts to ${url}; set HAPPY_MCP_TELEMETRY=off to disable)`
      : 'Telemetry: off';
  }

  return {
    enabled,
    url,
    record,
    flush,
    stop,
    setTransport,
    buildPing,
    describe,
    get size() { return counts.size; }
  };
}

let singleton = null;

/** Process-wide tracker, created on first use from process.env. */
export function defaultUsageTracker() {
  if (!singleton) singleton = createUsageTracker();
  return singleton;
}

/** Test hook: drop the process-wide tracker so the next call rebuilds it. */
export function resetDefaultUsageTracker() {
  if (singleton) singleton.stop();
  singleton = null;
}
