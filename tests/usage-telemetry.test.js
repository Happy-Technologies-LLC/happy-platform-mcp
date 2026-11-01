import { describe, expect, test, jest } from '@jest/globals';
import { createUsageTracker, telemetryEnabled, telemetryUrl } from '../src/usage-telemetry.js';
import { TELEMETRY_URL } from '../src/project-links.js';
import { PACKAGE_NAME, PACKAGE_VERSION } from '../src/package-info.js';

function fakeProcess() {
  const listeners = {};
  return {
    listeners,
    once(event, fn) { listeners[event] = fn; }
  };
}

function trackerWith(env, overrides = {}) {
  const fetchImpl = jest.fn(async () => ({ ok: true }));
  const proc = fakeProcess();
  const timers = [];
  const tracker = createUsageTracker({
    env,
    fetchImpl,
    proc,
    schedule: (fn, ms) => { const t = { fn, ms, unref: jest.fn() }; timers.push(t); return t; },
    unschedule: jest.fn(),
    now: () => new Date('2026-11-01T15:00:00.000Z'),
    platform: 'linux',
    nodeVersion: 'v22.4.1',
    sessionId: '00000000-0000-4000-8000-000000000000',
    ...overrides
  });
  return { tracker, fetchImpl, proc, timers };
}

describe('telemetryEnabled', () => {
  test('is off by default and off for anything that is not an explicit on-value', () => {
    expect(telemetryEnabled({})).toBe(false);
    expect(telemetryEnabled({ HAPPY_MCP_TELEMETRY: '' })).toBe(false);
    expect(telemetryEnabled({ HAPPY_MCP_TELEMETRY: 'off' })).toBe(false);
    expect(telemetryEnabled({ HAPPY_MCP_TELEMETRY: 'false' })).toBe(false);
    expect(telemetryEnabled({ HAPPY_MCP_TELEMETRY: 'maybe' })).toBe(false);
  });

  test('accepts on, 1, true and yes in any case', () => {
    for (const value of ['on', '1', 'true', 'YES', ' On ']) {
      expect(telemetryEnabled({ HAPPY_MCP_TELEMETRY: value })).toBe(true);
    }
  });
});

describe('telemetryUrl', () => {
  test('defaults to the project collector and honours an override', () => {
    expect(telemetryUrl({})).toBe(TELEMETRY_URL);
    expect(telemetryUrl({ HAPPY_MCP_TELEMETRY_URL: ' https://example.test/ping ' })).toBe('https://example.test/ping');
  });
});

describe('createUsageTracker when off', () => {
  test('records nothing, schedules nothing, sends nothing', async () => {
    const { tracker, fetchImpl, timers, proc } = trackerWith({});
    tracker.record('SN-Query-Table');
    tracker.record('SN-Query-Table');
    expect(tracker.enabled).toBe(false);
    expect(tracker.size).toBe(0);
    expect(await tracker.flush()).toBe(false);
    expect(fetchImpl).not.toHaveBeenCalled();
    expect(timers).toHaveLength(0);
    expect(proc.listeners.beforeExit).toBeUndefined();
    expect(tracker.describe()).toBe('Telemetry: off');
  });
});

describe('createUsageTracker when on', () => {
  test('counts tool names and posts only the documented fields', async () => {
    const { tracker, fetchImpl } = trackerWith({ HAPPY_MCP_TELEMETRY: 'on' });
    tracker.setTransport('stdio');
    tracker.record('SN-Query-Table');
    tracker.record('SN-Query-Table');
    tracker.record('SN-Create-Record');
    tracker.record('');
    tracker.record(undefined);

    expect(tracker.buildPing()).toEqual({
      v: 1,
      package: PACKAGE_NAME,
      version: PACKAGE_VERSION,
      session: '00000000-0000-4000-8000-000000000000',
      transport: 'stdio',
      node: 'v22',
      platform: 'linux',
      tools: { 'SN-Create-Record': 1, 'SN-Query-Table': 2, unknown: 2 },
      sent_at: '2026-11-01T15:00:00.000Z'
    });

    expect(await tracker.flush()).toBe(true);
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe(TELEMETRY_URL);
    expect(init.method).toBe('POST');
    const body = JSON.parse(init.body);
    expect(Object.keys(body).sort()).toEqual(['node', 'package', 'platform', 'sent_at', 'session', 'tools', 'transport', 'v', 'version']);
    expect(JSON.stringify(body)).not.toMatch(/service-now\.com|username|password|instance/i);
    // Counts reset after a send.
    expect(tracker.size).toBe(0);
    expect(await tracker.flush()).toBe(false);
  });

  test('schedules an unref\'d periodic flush and a beforeExit flush on first record', () => {
    const { tracker, timers, proc } = trackerWith({ HAPPY_MCP_TELEMETRY: '1' });
    tracker.record('SN-Get-Record');
    expect(timers).toHaveLength(1);
    expect(timers[0].ms).toBe(10 * 60 * 1000);
    expect(timers[0].unref).toHaveBeenCalled();
    expect(typeof proc.listeners.beforeExit).toBe('function');
    tracker.record('SN-Get-Record');
    expect(timers).toHaveLength(1);
  });

  test('swallows send failures and never throws into the tool call', async () => {
    const fetchImpl = jest.fn(async () => { throw new Error('network down'); });
    const { tracker } = trackerWith({ HAPPY_MCP_TELEMETRY: 'yes' }, { fetchImpl });
    tracker.record('SN-Query-Table');
    await expect(tracker.flush()).resolves.toBe(false);
    expect(fetchImpl).toHaveBeenCalledTimes(1);
  });

  test('uses the override URL and says so', () => {
    const { tracker } = trackerWith({ HAPPY_MCP_TELEMETRY: 'true', HAPPY_MCP_TELEMETRY_URL: 'https://collector.example/v1' });
    expect(tracker.url).toBe('https://collector.example/v1');
    expect(tracker.describe()).toContain('https://collector.example/v1');
    expect(tracker.describe()).toContain('HAPPY_MCP_TELEMETRY=off');
  });
});
