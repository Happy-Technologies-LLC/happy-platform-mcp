/**
 * ATF tools over the sn_cicd REST API: real ServiceNowClient against a local
 * fake ServiceNow HTTP endpoint. No live instance is contacted.
 */
import { afterEach, beforeEach, describe, expect, jest, test } from '@jest/globals';
import http from 'node:http';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { createMcpServer } from '../src/mcp-server-consolidated.js';
import { ServiceNowClient } from '../src/servicenow-client.js';
import { handleAtfTool } from '../src/cicd-atf.js';

const PASSWORD = 'fake-password-xyz';
const TEST_ID = '0123456789abcdef0123456789abcdef';
const SUITE_ID = '1123456789abcdef0123456789abcdef';
const PROGRESS_ID = '2123456789abcdef0123456789abcdef';
const RESULT_ID = '3123456789abcdef0123456789abcdef';

let fake;

beforeEach(() => {
  jest.spyOn(console, 'error').mockImplementation(() => {});
});

afterEach(async () => {
  jest.restoreAllMocks();
  if (fake) await fake.close();
  fake = null;
});

/** routes(req) -> [status, body] | undefined; body string = raw text/html. */
async function startFake(routes) {
  const requests = [];
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1');
    const entry = { method: req.method, path: url.pathname, query: Object.fromEntries(url.searchParams) };
    requests.push(entry);
    const answer = routes(entry) || [404, { error: { message: `unexpected ${req.method} ${url.pathname}` } }];
    const [status, body] = answer;
    if (typeof body === 'string') {
      res.writeHead(status, { 'content-type': 'text/html' });
      return res.end(body);
    }
    res.writeHead(status, { 'content-type': 'application/json' });
    return res.end(JSON.stringify(body));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  fake = {
    url: `http://127.0.0.1:${port}`,
    requests,
    close: () => new Promise((resolve) => server.close(resolve))
  };
  return fake;
}

function clientFor(f) {
  return new ServiceNowClient(f.url, 'fake-user', PASSWORD);
}

function fakeClock() {
  let t = 0;
  const sleeps = [];
  return {
    sleeps,
    deps: {
      now: () => t,
      sleep: async (ms) => { sleeps.push(ms); t += ms; }
    }
  };
}

function payloadOf(result) {
  return JSON.parse(result.content[0].text);
}

const started = [200, { result: { status: '0', links: { progress: { id: PROGRESS_ID, url: 'x' } } } }];

function progressSequence(...states) {
  let i = 0;
  return () => {
    const body = states[Math.min(i, states.length - 1)];
    i += 1;
    return [200, { result: body }];
  };
}

describe('SN-Run-ATF-Test', () => {
  test('runs by sys_id, polls to completion, and returns the test result', async () => {
    const progress = progressSequence(
      { status: '1', percent_complete: 40, status_message: 'Running' },
      { status: '2', percent_complete: 100, links: { results: { id: RESULT_ID, url: 'https://x/api/sn_cicd/tests/test/results/r' } } }
    );
    const f = await startFake((r) => {
      if (r.method === 'POST' && r.path === '/api/sn_cicd/tests/run_test') return started;
      if (r.path === `/api/sn_cicd/progress/${PROGRESS_ID}`) return progress();
      if (r.path === `/api/sn_cicd/tests/test/results/${RESULT_ID}`) {
        return [200, { result: { test_name: 'Login works', test_status: 'success', output: 'all good' } }];
      }
    });
    const clock = fakeClock();
    const result = await handleAtfTool('SN-Run-ATF-Test', { test_sys_id: TEST_ID, run_in_cloud: true }, clientFor(f), clock.deps);
    const body = payloadOf(result);

    expect(result.isError).toBe(false);
    expect(body).toMatchObject({
      status: 'successful',
      successful: true,
      progress_id: PROGRESS_ID,
      results: { result_type: 'test', result_id: RESULT_ID, test_name: 'Login works', test_status: 'success', output: 'all good' }
    });
    expect(body.results.links.record).toBe(`${f.url}/sys_atf_test_result.do?sys_id=${RESULT_ID}`);
    expect(body.results.links.api).toBe(`${f.url}/api/sn_cicd/tests/test/results/${RESULT_ID}`);
    const run = f.requests.find((r) => r.method === 'POST');
    expect(run.query).toEqual({ test_sys_id: TEST_ID, run_in_cloud: 'true' });
    expect(f.requests.some((r) => r.path.includes('/table/sys_atf_test'))).toBe(false);
    expect(clock.sleeps).toEqual([2000]);
  });

  test('resolves test_name to a sys_id, escaping encoded-query separators', async () => {
    const f = await startFake((r) => {
      if (r.path === '/api/now/table/sys_atf_test') return [200, { result: [{ sys_id: TEST_ID, name: 'a^b' }] }];
      if (r.method === 'POST') return started;
    });
    const body = payloadOf(await handleAtfTool('SN-Run-ATF-Test', { test_name: 'a^b', wait: false }, clientFor(f)));

    expect(body).toMatchObject({ status: 'started', progress_id: PROGRESS_ID, target: { test_sys_id: TEST_ID, test_name: 'a^b' } });
    expect(f.requests[0].query.sysparm_query).toBe('name=a^^b');
    expect(f.requests[1].query.test_sys_id).toBe(TEST_ID);
  });

  test.each([
    ['no match', [], /No ATF test found/],
    ['ambiguous', [{ sys_id: TEST_ID }, { sys_id: SUITE_ID }], /Multiple ATF tests found.*test_sys_id/]
  ])('rejects a test_name with %s and never starts a run', async (_label, rows, message) => {
    const f = await startFake((r) => (r.path === '/api/now/table/sys_atf_test' ? [200, { result: rows }] : started));
    await expect(handleAtfTool('SN-Run-ATF-Test', { test_name: 'Login' }, clientFor(f))).rejects.toThrow(message);
    expect(f.requests.some((r) => r.method === 'POST')).toBe(false);
  });

  test.each([
    ['both identifiers', { test_sys_id: TEST_ID, test_name: 'Login' }, /mutually exclusive/],
    ['no identifier', {}, /Provide either test_sys_id or test_name/],
    ['a malformed sys_id', { test_sys_id: 'not-a-sys-id' }, /test_sys_id must be a 32-character/],
    ['an excessive timeout', { test_sys_id: TEST_ID, timeout_seconds: 601 }, /timeout_seconds must be a number from 1 to 600/]
  ])('rejects %s before any request', async (_label, args, message) => {
    const f = await startFake(() => started);
    await expect(handleAtfTool('SN-Run-ATF-Test', args, clientFor(f))).rejects.toThrow(message);
    expect(f.requests).toHaveLength(0);
  });

  test('timeout returns the progress id and state instead of throwing', async () => {
    const f = await startFake((r) => {
      if (r.method === 'POST') return started;
      if (r.path === `/api/sn_cicd/progress/${PROGRESS_ID}`) return [200, { result: { status: '1', percent_complete: 10 } }];
    });
    const clock = fakeClock();
    const result = await handleAtfTool('SN-Run-ATF-Test', { test_sys_id: TEST_ID, timeout_seconds: 5, poll_interval_seconds: 2 }, clientFor(f), clock.deps);
    const body = payloadOf(result);

    expect(result.isError).toBe(false);
    expect(body).toMatchObject({ status: 'timeout', progress_id: PROGRESS_ID, progress: { state: 'running', percent_complete: 10, finished: false } });
    expect(body.next_step).toMatch(/SN-Get-CICD-Progress/);
    expect(clock.sleeps).toEqual([2000, 2000, 1000]);
  });

  test('a progress read failure after the run started still returns the progress id', async () => {
    const f = await startFake((r) => {
      if (r.method === 'POST') return started;
      if (r.path.startsWith('/api/sn_cicd/progress/')) return [500, { error: { message: 'boom' } }];
    });
    const result = await handleAtfTool('SN-Run-ATF-Test', { test_sys_id: TEST_ID }, clientFor(f), fakeClock().deps);

    expect(result.isError).toBe(true);
    expect(payloadOf(result)).toMatchObject({ status: 'unknown', progress_id: PROGRESS_ID });
  });
});

describe('SN-Run-ATF-Suite', () => {
  test('passes the suite name to the instance and reports counts and per-test status on failure', async () => {
    const f = await startFake((r) => {
      if (r.method === 'POST' && r.path === '/api/sn_cicd/testsuite/run') return started;
      if (r.path === `/api/sn_cicd/progress/${PROGRESS_ID}`) {
        return [200, { result: { status: '3', status_message: 'Test suite failed', links: { results: { id: RESULT_ID } } } }];
      }
      if (r.path === `/api/sn_cicd/testsuite/results/${RESULT_ID}`) {
        return [200, { result: {
          test_suite_name: 'Smoke', test_suite_status: 'failure',
          rolledup_test_success_count: 3, rolledup_test_failure_count: 1,
          rolledup_test_error_count: 0, rolledup_test_skip_count: 2
        } }];
      }
      if (r.path === '/api/now/table/sys_atf_test_result') {
        return [200, { result: [
          { sys_id: TEST_ID, test: 'Login works', status: 'Success' },
          { sys_id: SUITE_ID, test: 'Checkout', status: 'Failure', output: 'assert failed' }
        ] }];
      }
    });
    const result = await handleAtfTool('SN-Run-ATF-Suite', { test_suite_name: 'Smoke', browser_name: 'chrome' }, clientFor(f), fakeClock().deps);
    const body = payloadOf(result);

    expect(result.isError).toBe(true);
    expect(body).toMatchObject({
      status: 'failed',
      successful: false,
      results: {
        result_type: 'suite',
        test_suite_name: 'Smoke',
        counts: { passed: 3, failed: 1, errored: 0, skipped: 2 },
        tests: [
          { test_name: 'Login works', status: 'Success' },
          { test_name: 'Checkout', status: 'Failure', output: 'assert failed' }
        ]
      }
    });
    expect(f.requests.find((r) => r.method === 'POST').query).toEqual({ test_suite_name: 'Smoke', browser_name: 'chrome' });
    expect(body.results.links).toEqual({
      api: `${f.url}/api/sn_cicd/testsuite/results/${RESULT_ID}`,
      record: `${f.url}/sys_atf_test_suite_result.do?sys_id=${RESULT_ID}`
    });
    expect(f.requests.find((r) => r.path === '/api/now/table/sys_atf_test_result').query.sysparm_query).toMatch(new RegExp(`^parent=${RESULT_ID}`));
  });

  test('rejects both suite identifiers and an unknown browser before any request', async () => {
    const f = await startFake(() => started);
    const client = clientFor(f);
    await expect(handleAtfTool('SN-Run-ATF-Suite', { test_suite_sys_id: SUITE_ID, test_suite_name: 'Smoke' }, client)).rejects.toThrow(/mutually exclusive/);
    await expect(handleAtfTool('SN-Run-ATF-Suite', { test_suite_sys_id: SUITE_ID, browser_name: 'netscape' }, client)).rejects.toThrow(/browser_name must be one of/);
    expect(f.requests).toHaveLength(0);
  });
});

describe('sn_cicd availability errors', () => {
  test.each([
    ['400 unknown-URI', [400, { error: { message: 'Requested URI does not represent any resource', detail: null } }]],
    ['404 without a body', [404, {}]],
    ['an HTML page', [200, '<html><body>Log in</body></html>']]
  ])('names the CI/CD REST API plugin on %s', async (_label, answer) => {
    const f = await startFake(() => answer);
    const failure = handleAtfTool('SN-Run-ATF-Suite', { test_suite_sys_id: SUITE_ID }, clientFor(f));
    await expect(failure).rejects.toMatchObject({ code: 'CICD_PLUGIN_INACTIVE' });
    await expect(failure).rejects.toThrow(/com\.glide\.continuousdelivery/);
  });

  test('names the sn_cicd role on 403 without leaking credentials', async () => {
    const f = await startFake(() => [403, { error: { message: 'User Not Authorized', detail: `basic ${PASSWORD}` } }]);
    const error = await handleAtfTool('SN-Get-CICD-Progress', { progress_id: PROGRESS_ID }, clientFor(f)).catch((e) => e);

    expect(error.code).toBe('CICD_ROLE_MISSING');
    expect(error.message).toMatch(/sn_cicd\.sys_ci_automation/);
    expect(error.message).not.toContain(PASSWORD);
  });

  test('reports an sn_cicd operation error as itself, not as a missing plugin', async () => {
    const f = await startFake(() => [400, { result: { error: 'Invalid test_sys_id', status: '3' } }]);
    const error = await handleAtfTool('SN-Run-ATF-Test', { test_sys_id: TEST_ID }, clientFor(f)).catch((e) => e);

    expect(error.code).toBe('CICD_REQUEST_FAILED');
    expect(error.message).toMatch(/Invalid test_sys_id/);
  });

  test.each([
    ['SN-Run-ATF-Test', { test_sys_id: TEST_ID }, 'Start ATF test Failed with the given test ID'],
    ['SN-Run-ATF-Suite', { test_suite_sys_id: SUITE_ID }, 'Start ATF test suite failed with the given test suite ID']
  ])('%s start failure hints that the ATF runner may be disabled', async (tool, args, message) => {
    const f = await startFake(() => [400, { error: { message, detail: null } }]);
    const error = await handleAtfTool(tool, args, clientFor(f)).catch((e) => e);

    expect(error.code).toBe('ATF_RUNNER_DISABLED');
    expect(error.message).toContain(message);
    expect(error.message).toMatch(/may be disabled.*sn_atf\.runner\.enabled.*must be true/);
  });
});

describe('SN-Get-CICD-Progress and SN-Get-ATF-Results', () => {
  test('single progress read reports state and the results id without polling', async () => {
    const f = await startFake(() => [200, { result: { status: '2', percent_complete: 100, links: { results: { id: RESULT_ID } } } }]);
    const clock = fakeClock();
    const body = payloadOf(await handleAtfTool('SN-Get-CICD-Progress', { progress_id: PROGRESS_ID }, clientFor(f), clock.deps));

    expect(body).toMatchObject({ progress_id: PROGRESS_ID, state: 'successful', finished: true, results: { id: RESULT_ID } });
    expect(f.requests).toHaveLength(1);
    expect(clock.sleeps).toEqual([]);
  });

  test('requires a valid result_type', async () => {
    const f = await startFake(() => [200, { result: {} }]);
    await expect(handleAtfTool('SN-Get-ATF-Results', { result_id: RESULT_ID, result_type: 'all' }, clientFor(f))).rejects.toThrow(/result_type must be one of: test, suite/);
    expect(f.requests).toHaveLength(0);
  });
});

describe('SN-Get-ATF-Test-Logs', () => {
  test('sends the pattern and limit to the instance and returns the matching lines', async () => {
    const f = await startFake((r) => (r.path === `/api/sn_cicd/tests/test/results/${RESULT_ID}/logs`
      ? [200, { result: { logs: ['ERROR step 2 failed'] } }]
      : undefined));
    const body = payloadOf(await handleAtfTool('SN-Get-ATF-Test-Logs', { result_id: RESULT_ID, pattern: 'error|warn', limit: 20 }, clientFor(f)));

    expect(f.requests[0].query).toEqual({ sysparm_regex: 'error|warn', sysparm_limit: '20' });
    expect(body).toMatchObject({ result_id: RESULT_ID, pattern: 'error|warn', limit: 20, logs: { logs: ['ERROR step 2 failed'] } });
  });

  test('a "No logs found" 404 is an empty, successful result', async () => {
    const f = await startFake(() => [404, { error: { message: `No logs found for test result ${RESULT_ID}`, detail: null } }]);
    const result = await handleAtfTool('SN-Get-ATF-Test-Logs', { result_id: RESULT_ID }, clientFor(f));

    expect(result.isError).toBe(false);
    expect(payloadOf(result)).toMatchObject({ result_id: RESULT_ID, limit: 100, logs: [] });
    expect(payloadOf(result).note).toMatch(/capture_node_logs: true/);
  });

  test.each([
    ['another 404 message', [404, { error: { message: 'No Record found', detail: null } }], 'CICD_REQUEST_FAILED'],
    ['a 404 without a body', [404, {}], 'CICD_PLUGIN_INACTIVE']
  ])('keeps %s as an error', async (_label, answer, code) => {
    const f = await startFake(() => answer);
    await expect(handleAtfTool('SN-Get-ATF-Test-Logs', { result_id: RESULT_ID }, clientFor(f))).rejects.toMatchObject({ code });
  });

  test.each([
    ['an invalid regex', { pattern: '(unclosed' }, /not a valid regular expression/],
    ['an oversized pattern', { pattern: 'a'.repeat(257) }, /at most 256 characters/],
    ['a nested quantifier', { pattern: '(a+)+$' }, /nested quantifier/],
    ['a limit over the cap', { limit: 1001 }, /limit must be an integer from 1 to 1000/]
  ])('rejects %s before any request', async (_label, extra, message) => {
    const f = await startFake(() => [200, { result: {} }]);
    await expect(handleAtfTool('SN-Get-ATF-Test-Logs', { result_id: RESULT_ID, ...extra }, clientFor(f))).rejects.toThrow(message);
    expect(f.requests).toHaveLength(0);
  });
});

describe('MCP registration', () => {
  test('lists the ATF tools with per-call instance routing and returns errors as tool errors', async () => {
    const f = await startFake(() => [200, { result: { status: '1', percent_complete: 5 } }]);
    const server = await createMcpServer(clientFor(f), {
      configManager: { getInstance: jest.fn(), listInstances: jest.fn(() => []) },
      createServiceNowClient: jest.fn()
    });
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    const mcp = new Client({ name: 'cicd-atf-test', version: '1.0.0' });
    await server.connect(serverTransport);
    await mcp.connect(clientTransport);
    try {
      const { tools } = await mcp.listTools();
      const atf = tools.filter((t) => /ATF|CICD/.test(t.name)).map((t) => t.name).sort();
      expect(atf).toEqual(['SN-Get-ATF-Results', 'SN-Get-ATF-Test-Logs', 'SN-Get-CICD-Progress', 'SN-Run-ATF-Suite', 'SN-Run-ATF-Test']);
      for (const tool of tools.filter((t) => atf.includes(t.name))) {
        expect(tool.inputSchema.properties.instance).toBeDefined();
      }

      const progress = await mcp.callTool({ name: 'SN-Get-CICD-Progress', arguments: { progress_id: PROGRESS_ID } });
      expect(JSON.parse(progress.content[0].text)).toMatchObject({ state: 'running', finished: false });

      const bad = await mcp.callTool({ name: 'SN-Get-ATF-Test-Logs', arguments: { result_id: RESULT_ID, pattern: '(' } });
      expect(bad.isError).toBe(true);
      expect(bad.content[0].text).toMatch(/^Error: pattern is not a valid regular expression/);
    } finally {
      await mcp.close();
      await server.close();
    }
  });
});
