/**
 * Happy MCP Server - Automated Test Framework (ATF) tools over the sn_cicd REST API
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 *
 * sn_cicd protocol behavior (run_test / testsuite run dispatch, progress
 * polling, result and log endpoints, error-body shapes, test-name
 * resolution) ported from @servicenow/sdk-api 4.14.0, MIT.
 */

import { MAX_NAME_LENGTH, assertName, assertSysId } from './bulk-limits.js';

export const DEFAULT_WAIT_TIMEOUT_SECONDS = 45;
export const MAX_WAIT_TIMEOUT_SECONDS = 600;
export const DEFAULT_POLL_INTERVAL_SECONDS = 2;
export const MIN_POLL_INTERVAL_SECONDS = 1;
export const MAX_POLL_INTERVAL_SECONDS = 30;
export const DEFAULT_LOG_LIMIT = 100;
export const MAX_LOG_LIMIT = 1000;
export const MAX_LOG_PATTERN_LENGTH = 256;
export const MAX_SUITE_TEST_ROWS = 200;
const MAX_OPTION_LENGTH = 100;
const MAX_OUTPUT_FIELD_CHARS = 2000;
const MAX_ERROR_DETAIL_CHARS = 500;
const MAX_LOG_PAYLOAD_CHARS = 256 * 1024;

const CICD_PLUGIN_ID = 'com.glide.continuousdelivery';
const CICD_ROLE = 'sn_cicd.sys_ci_automation';

/** `GET /api/sn_cicd/progress/{id}` status codes. */
const PROGRESS_STATES = Object.freeze({
  0: 'pending',
  1: 'running',
  2: 'successful',
  3: 'failed',
  4: 'canceled'
});
const TERMINAL_STATES = new Set(['successful', 'failed', 'canceled']);

const BROWSER_NAMES = ['any', 'chrome', 'firefox', 'edge', 'ie', 'safari'];
const RESULT_TYPES = ['test', 'suite'];

const WAIT_PROPERTIES = Object.freeze({
  wait: {
    type: 'boolean',
    description: 'If true (default), poll the sn_cicd progress tracker until the run finishes or timeout_seconds elapses, then return pass/fail results. If false, return the progress_id immediately.',
    default: true
  },
  timeout_seconds: {
    type: 'number',
    description: `Maximum seconds to wait when wait is true (default ${DEFAULT_WAIT_TIMEOUT_SECONDS}, max ${MAX_WAIT_TIMEOUT_SECONDS}). On timeout the run keeps going on the instance; the response carries progress_id for SN-Get-CICD-Progress. Many MCP clients cancel requests after about 60 seconds.`,
    minimum: 1,
    maximum: MAX_WAIT_TIMEOUT_SECONDS,
    default: DEFAULT_WAIT_TIMEOUT_SECONDS
  },
  poll_interval_seconds: {
    type: 'number',
    description: `Seconds between progress polls (default ${DEFAULT_POLL_INTERVAL_SECONDS}, ${MIN_POLL_INTERVAL_SECONDS}-${MAX_POLL_INTERVAL_SECONDS}).`,
    minimum: MIN_POLL_INTERVAL_SECONDS,
    maximum: MAX_POLL_INTERVAL_SECONDS,
    default: DEFAULT_POLL_INTERVAL_SECONDS
  }
});

const SYS_ID_SCHEMA = Object.freeze({ type: 'string', pattern: '^[0-9a-f]{32}$' });

/** Fresh definitions per call: the server mutates schemas to add `instance`. */
export function createAtfToolDefinitions() {
  return [
    {
      name: 'SN-Run-ATF-Test',
      description: `Run one Automated Test Framework test on the instance through the CI/CD REST API (POST /api/sn_cicd/tests/run_test) and, by default, wait for its pass/fail result. Provide exactly one of test_sys_id or test_name (a name must match exactly one sys_atf_test record). This EXECUTES the test, which may create, change, or roll back data. Requires the CI/CD REST API plugin (${CICD_PLUGIN_ID}) and the ${CICD_ROLE} or admin role.`,
      inputSchema: {
        type: 'object',
        properties: {
          test_sys_id: { ...SYS_ID_SCHEMA, description: 'sys_id of the ATF test (sys_atf_test). Mutually exclusive with test_name.' },
          test_name: { type: 'string', maxLength: MAX_NAME_LENGTH, description: 'Exact name of the ATF test; resolved to a sys_id first. Fails when zero or several tests share the name. Mutually exclusive with test_sys_id.' },
          run_in_cloud: { type: 'boolean', description: 'Run UI steps on the ATF Cloud Runner instead of a scheduled client test runner.' },
          capture_node_logs: { type: 'boolean', description: 'Capture node (browser) logs during the run.' },
          ...WAIT_PROPERTIES
        }
      }
    },
    {
      name: 'SN-Run-ATF-Suite',
      description: `Run an Automated Test Framework test suite through the CI/CD REST API (POST /api/sn_cicd/testsuite/run) and, by default, wait for pass/fail/error/skip counts and per-test status. Provide exactly one of test_suite_sys_id or test_suite_name. This EXECUTES every test in the suite, which may create, change, or roll back data. Requires the CI/CD REST API plugin (${CICD_PLUGIN_ID}) and the ${CICD_ROLE} or admin role.`,
      inputSchema: {
        type: 'object',
        properties: {
          test_suite_sys_id: { ...SYS_ID_SCHEMA, description: 'sys_id of the test suite (sys_atf_test_suite). Mutually exclusive with test_suite_name.' },
          test_suite_name: { type: 'string', maxLength: MAX_NAME_LENGTH, description: 'Name of the test suite; the instance resolves it. Mutually exclusive with test_suite_sys_id.' },
          browser_name: { type: 'string', enum: BROWSER_NAMES, description: 'Browser to run UI tests in.' },
          browser_version: { type: 'string', maxLength: MAX_OPTION_LENGTH, description: 'Browser version to run UI tests in.' },
          os_name: { type: 'string', maxLength: MAX_OPTION_LENGTH, description: 'Operating system to run UI tests on.' },
          os_version: { type: 'string', maxLength: MAX_OPTION_LENGTH, description: 'Operating system version to run UI tests on.' },
          run_in_cloud: { type: 'boolean', description: 'Run the suite on the ATF Cloud Runner instead of a manual or scheduled client test runner.' },
          is_performance_run: { type: 'boolean', description: 'Run the suite as a performance test instead of a functional test.' },
          ...WAIT_PROPERTIES
        }
      }
    },
    {
      name: 'SN-Get-CICD-Progress',
      description: 'Read a CI/CD REST API progress tracker (GET /api/sn_cicd/progress/{progress_id}), such as the progress_id returned by SN-Run-ATF-Test or SN-Run-ATF-Suite. Returns state (pending, running, successful, failed, canceled), percent complete, messages, and the results id once finished. Read-only: it never starts work. Set wait to keep polling until the tracker finishes.',
      inputSchema: {
        type: 'object',
        properties: {
          progress_id: { ...SYS_ID_SCHEMA, description: 'Progress tracker sys_id returned by a run tool.' },
          wait: {
            type: 'boolean',
            description: 'If true, poll until the tracker finishes or timeout_seconds elapses. Default false (single read).',
            default: false
          },
          timeout_seconds: WAIT_PROPERTIES.timeout_seconds,
          poll_interval_seconds: WAIT_PROPERTIES.poll_interval_seconds
        },
        required: ['progress_id']
      }
    },
    {
      name: 'SN-Get-ATF-Results',
      description: 'Read an ATF run result through the CI/CD REST API. result_type "test" reads GET /api/sn_cicd/tests/test/results/{result_id} (status, output, links). result_type "suite" reads GET /api/sn_cicd/testsuite/results/{result_id} (pass/fail/error/skip counts, child suites, links) plus per-test status from sys_atf_test_result. Read-only.',
      inputSchema: {
        type: 'object',
        properties: {
          result_id: { ...SYS_ID_SCHEMA, description: 'Result sys_id: results.id from a run tool or SN-Get-CICD-Progress.' },
          result_type: { type: 'string', enum: RESULT_TYPES, description: '"test" for a single-test result (sys_atf_test_result), "suite" for a test suite result (sys_atf_test_suite_result).' }
        },
        required: ['result_id', 'result_type']
      }
    },
    {
      name: 'SN-Get-ATF-Test-Logs',
      description: `Read the logs captured for one ATF test result (GET /api/sn_cicd/tests/test/results/{result_id}/logs). An optional regular expression filters log lines on the instance, which always matches case-insensitively. Read-only.`,
      inputSchema: {
        type: 'object',
        properties: {
          result_id: { ...SYS_ID_SCHEMA, description: 'sys_id of the ATF test result (sys_atf_test_result).' },
          pattern: {
            type: 'string',
            maxLength: MAX_LOG_PATTERN_LENGTH,
            description: `Optional regular expression (at most ${MAX_LOG_PATTERN_LENGTH} characters); only matching lines are returned. Invalid expressions and nested quantifiers such as (a+)+ are rejected.`
          },
          limit: {
            type: 'integer',
            description: `Maximum log lines to return (default ${DEFAULT_LOG_LIMIT}, max ${MAX_LOG_LIMIT}).`,
            minimum: 1,
            maximum: MAX_LOG_LIMIT,
            default: DEFAULT_LOG_LIMIT
          }
        },
        required: ['result_id']
      }
    }
  ];
}

export const ATF_TOOL_NAMES = new Set(createAtfToolDefinitions().map((tool) => tool.name));

export function isAtfTool(name) {
  return ATF_TOOL_NAMES.has(name);
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

function present(value) {
  return value !== undefined && value !== null && value !== '';
}

function assertExactlyOneOf(args, a, b) {
  const hasA = present(args[a]);
  const hasB = present(args[b]);
  if (!hasA && !hasB) throw new Error(`Provide either ${a} or ${b}`);
  if (hasA && hasB) throw new Error(`${a} and ${b} are mutually exclusive; provide only one`);
}

function optionalBoolean(args, field) {
  const value = args[field];
  if (value === undefined || value === null) return undefined;
  if (typeof value !== 'boolean') throw new Error(`${field} must be a boolean`);
  return value;
}

function optionalString(args, field) {
  const value = args[field];
  if (!present(value)) return undefined;
  if (typeof value !== 'string' || value.length > MAX_OPTION_LENGTH) {
    throw new Error(`${field} must be a string of at most ${MAX_OPTION_LENGTH} characters`);
  }
  return value;
}

function boundedNumber(args, field, { min, max, fallback }) {
  const value = args[field];
  if (value === undefined || value === null) return fallback;
  if (typeof value !== 'number' || !Number.isFinite(value) || value < min || value > max) {
    throw new Error(`${field} must be a number from ${min} to ${max}`);
  }
  return value;
}

function waitOptions(args, defaultWait) {
  return {
    wait: optionalBoolean(args, 'wait') ?? defaultWait,
    timeoutMs: boundedNumber(args, 'timeout_seconds', {
      min: 1,
      max: MAX_WAIT_TIMEOUT_SECONDS,
      fallback: DEFAULT_WAIT_TIMEOUT_SECONDS
    }) * 1000,
    intervalMs: boundedNumber(args, 'poll_interval_seconds', {
      min: MIN_POLL_INTERVAL_SECONDS,
      max: MAX_POLL_INTERVAL_SECONDS,
      fallback: DEFAULT_POLL_INTERVAL_SECONDS
    }) * 1000
  };
}

/**
 * Heuristic guard against catastrophic backtracking: rejects a quantified
 * group whose body is itself quantified, e.g. (a+)+, (.*)*, (\w+){2,}.
 */
function hasNestedQuantifier(pattern) {
  const src = pattern.replace(/\\./g, 'x').replace(/\[[^\]]*\]/g, 'x');
  const stack = [];
  for (let i = 0; i < src.length; i += 1) {
    const ch = src[i];
    if (ch === '(') {
      stack.push(false);
    } else if (ch === ')') {
      const innerQuantified = stack.pop() ?? false;
      const next = src[i + 1];
      const groupQuantified = next === '*' || next === '+' || next === '{';
      if (innerQuantified && groupQuantified) return true;
      if (stack.length > 0 && (innerQuantified || groupQuantified)) stack[stack.length - 1] = true;
    } else if ((ch === '*' || ch === '+' || ch === '{') && stack.length > 0) {
      stack[stack.length - 1] = true;
    }
  }
  return false;
}

export function validateLogPattern(pattern) {
  if (!present(pattern)) return undefined;
  if (typeof pattern !== 'string') throw new Error('pattern must be a string');
  if (pattern.length > MAX_LOG_PATTERN_LENGTH) {
    throw new Error(`pattern must be at most ${MAX_LOG_PATTERN_LENGTH} characters (received ${pattern.length})`);
  }
  if (/[\u0000-\u001f\u007f]/.test(pattern)) {
    throw new Error('pattern must not contain control characters');
  }
  try {
    new RegExp(pattern, 'i');
  } catch (error) {
    throw new Error(`pattern is not a valid regular expression: ${error.message}`);
  }
  if (hasNestedQuantifier(pattern)) {
    throw new Error('pattern contains a nested quantifier such as (a+)+, which can backtrack catastrophically; simplify the expression');
  }
  return pattern;
}

// ---------------------------------------------------------------------------
// sn_cicd HTTP + error mapping
// ---------------------------------------------------------------------------

function truncate(value, max) {
  if (typeof value !== 'string') return value;
  return value.length > max ? `${value.slice(0, max)}… (truncated)` : value;
}

function nonEmptyString(value) {
  return typeof value === 'string' && value.trim() !== '' ? value : undefined;
}

/**
 * sn_cicd error bodies vary: `{error: {message, detail}}` from the platform
 * REST layer, `{result: {error}}` or `{result: {status_message}}` from
 * sn_cicd operations.
 */
function extractErrorMessage(body) {
  if (typeof body === 'string') return nonEmptyString(body.replace(/<[^>]*>/g, ' ').replace(/\s+/g, ' ').trim());
  const generic = body?.error;
  const result = body?.result;
  return nonEmptyString(generic?.message)
    ?? nonEmptyString(typeof generic === 'string' ? generic : undefined)
    ?? nonEmptyString(result?.error)
    ?? nonEmptyString(result?.status_message);
}

const UNKNOWN_API_PATTERN = /does not represent any resource|no such api|api (?:was )?not found|invalid api|not a valid (?:api|resource)/i;
const RUN_PATHS = new Set(['/api/sn_cicd/tests/run_test', '/api/sn_cicd/testsuite/run']);
// e.g. "Start ATF test Failed with the given test ID" when sn_atf.runner.enabled=false.
const START_FAILED_PATTERN = /\bstart\b.*\bfail/i;
const NO_LOGS_PATTERN = /^\s*No logs found for test result\b/i;

export class CicdApiError extends Error {
  constructor(message, { code, status, detail } = {}) {
    super(message);
    this.name = 'CicdApiError';
    this.code = code;
    if (status !== undefined) this.status = status;
    if (detail !== undefined) this.detail = detail;
  }
}

function toCicdError(error, path) {
  if (error instanceof CicdApiError) return error;
  const status = error?.status ?? error?.response?.status;
  const body = error?.response?.data;
  const detail = truncate(extractErrorMessage(body), MAX_ERROR_DETAIL_CHARS);
  const said = detail ? ` ServiceNow said: ${detail}` : '';

  if (status === 401) {
    return new CicdApiError(`ServiceNow rejected the credentials (401) for ${path}. Check the instance credentials.${said}`, { code: 'CICD_UNAUTHORIZED', status });
  }
  if (status === 403) {
    return new CicdApiError(
      `Access denied (403) to the CI/CD REST API at ${path}. The ServiceNow user needs the ${CICD_ROLE} role (or admin). Grant the role to this integration user, then retry.${said}`,
      { code: 'CICD_ROLE_MISSING', status }
    );
  }
  if (status === 400 && RUN_PATHS.has(path) && detail && START_FAILED_PATTERN.test(detail)) {
    return new CicdApiError(
      `The instance refused to start the run (400 for ${path}).${said}${/[.!?]$/.test(said) || !said ? '' : '.'} ATF test execution may be disabled: check the sys_properties \`sn_atf.runner.enabled\` (must be true). The same message can also mean the test or suite id cannot be started.`,
      { code: 'ATF_RUNNER_DISABLED', status, detail }
    );
  }
  if ((status === 400 || status === 404) && (!detail || UNKNOWN_API_PATTERN.test(detail))) {
    return new CicdApiError(
      `The CI/CD REST API is not available on this instance (${status} for ${path}). Activate the "CI/CD REST API" plugin (${CICD_PLUGIN_ID}) under System Definition > Plugins, then retry.${said}`,
      { code: 'CICD_PLUGIN_INACTIVE', status }
    );
  }
  if (status !== undefined) {
    return new CicdApiError(`sn_cicd request to ${path} failed (${status}).${said || ` ${truncate(error?.message, MAX_ERROR_DETAIL_CHARS)}`}`, { code: 'CICD_REQUEST_FAILED', status, detail });
  }
  return new CicdApiError(`sn_cicd request to ${path} failed: ${truncate(error?.message || 'network error', MAX_ERROR_DETAIL_CHARS)}`, { code: 'CICD_REQUEST_FAILED' });
}

function httpClient(client) {
  if (!client?.client || typeof client.client.request !== 'function') {
    throw new Error('ServiceNow client does not support sn_cicd requests');
  }
  return client.client;
}

function withQuery(path, fields) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(fields)) {
    if (value !== undefined) query.set(key, String(value));
  }
  const qs = query.toString();
  return qs ? `${path}?${qs}` : path;
}

/** Unwraps the sn_cicd `{result}` envelope; maps failures to CicdApiError. */
async function cicdRequest(client, method, path, query = {}) {
  let response;
  try {
    response = await httpClient(client).request({ method, url: withQuery(path, query) });
  } catch (error) {
    throw toCicdError(error, path);
  }
  const body = response?.data;
  if (!body || typeof body !== 'object') {
    // An HTML login/landing page instead of JSON means the API route is missing.
    throw new CicdApiError(
      `The CI/CD REST API at ${path} returned a non-JSON response; the "CI/CD REST API" plugin (${CICD_PLUGIN_ID}) is probably not active. Activate it under System Definition > Plugins, then retry.`,
      { code: 'CICD_PLUGIN_INACTIVE', status: response?.status }
    );
  }
  return body.result ?? body;
}

// ---------------------------------------------------------------------------
// Progress tracking
// ---------------------------------------------------------------------------

function stateOf(status) {
  return PROGRESS_STATES[String(status)] ?? `unknown:${status}`;
}

function linkOf(links, key) {
  const link = links?.[key];
  if (!link || typeof link !== 'object') return undefined;
  const out = {};
  if (typeof link.id === 'string') out.id = link.id;
  if (typeof link.url === 'string') out.url = link.url;
  return Object.keys(out).length ? out : undefined;
}

function snapshotOf(progressId, result) {
  const state = stateOf(result?.status);
  const snapshot = {
    progress_id: progressId,
    state,
    status: result?.status,
    status_label: result?.status_label,
    status_message: result?.status_message,
    status_detail: result?.status_detail,
    error: nonEmptyString(result?.error),
    percent_complete: result?.percent_complete,
    finished: TERMINAL_STATES.has(state)
  };
  const results = linkOf(result?.links, 'results');
  if (results) snapshot.results = results;
  return Object.fromEntries(Object.entries(snapshot).filter(([, v]) => v !== undefined));
}

async function fetchProgress(client, progressId) {
  const result = await cicdRequest(client, 'GET', `/api/sn_cicd/progress/${progressId}`);
  return snapshotOf(progressId, result);
}

const realSleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Polls until terminal or the deadline. Never sleeps past the deadline.
 * Returns `{ snapshot, timedOut }`.
 */
async function pollProgress(client, progressId, { timeoutMs, intervalMs }, deps) {
  const sleep = deps.sleep || realSleep;
  const now = deps.now || Date.now;
  const deadline = now() + timeoutMs;
  let attempt = 0;
  while (true) {
    const snapshot = await fetchProgress(client, progressId);
    attempt += 1;
    client.notifyProgress?.(
      `sn_cicd ${progressId}: ${snapshot.state}${snapshot.percent_complete !== undefined ? ` (${snapshot.percent_complete}%)` : ''}${snapshot.status_message ? ` ${snapshot.status_message}` : ''}`,
      attempt
    );
    if (snapshot.finished) return { snapshot, timedOut: false };
    const remaining = deadline - now();
    if (remaining <= 0) return { snapshot, timedOut: true };
    await sleep(Math.min(intervalMs, remaining));
  }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

function instanceBase(client) {
  return typeof client?.instanceUrl === 'string' ? client.instanceUrl.replace(/\/+$/, '') : undefined;
}

function recordUrl(client, table, sysId) {
  const base = instanceBase(client);
  return base ? `${base}/${table}.do?sys_id=${sysId}` : undefined;
}

function apiUrl(client, path) {
  const base = instanceBase(client);
  return base ? `${base}${path}` : undefined;
}

function compact(object) {
  return Object.fromEntries(Object.entries(object).filter(([, v]) => v !== undefined));
}

async function getTestResult(client, resultId) {
  const path = `/api/sn_cicd/tests/test/results/${resultId}`;
  const result = await cicdRequest(client, 'GET', path);
  return compact({
    result_type: 'test',
    result_id: resultId,
    test_name: result.test_name,
    test_status: result.test_status,
    output: truncate(result.output, MAX_OUTPUT_FIELD_CHARS),
    start_time: result.start_time,
    end_time: result.end_time,
    duration: result.test_duration ?? result.run_time,
    links: compact({
      api: apiUrl(client, path),
      record: recordUrl(client, 'sys_atf_test_result', resultId)
    })
  });
}

function displayValue(value) {
  if (value && typeof value === 'object' && 'display_value' in value) return value.display_value;
  return value;
}

/** Best-effort per-test rows for a suite result; never fails the call. */
async function getSuiteTestRows(client, suiteResultId) {
  const path = '/api/now/table/sys_atf_test_result';
  try {
    const rows = await httpClient(client).get(withQuery(path, {
      sysparm_query: `parent=${suiteResultId}^ORDERBYstart_time`,
      sysparm_fields: 'sys_id,test,status,output,start_time,end_time,run_time',
      sysparm_display_value: 'true',
      sysparm_exclude_reference_link: 'true',
      sysparm_limit: MAX_SUITE_TEST_ROWS + 1
    }));
    const records = Array.isArray(rows?.data?.result) ? rows.data.result : [];
    return {
      tests: records.slice(0, MAX_SUITE_TEST_ROWS).map((row) => compact({
        test_result_sys_id: displayValue(row.sys_id),
        test_name: displayValue(row.test),
        status: displayValue(row.status),
        output: truncate(displayValue(row.output), MAX_OUTPUT_FIELD_CHARS),
        start_time: displayValue(row.start_time),
        end_time: displayValue(row.end_time),
        run_time: displayValue(row.run_time),
        link: recordUrl(client, 'sys_atf_test_result', displayValue(row.sys_id))
      })),
      tests_truncated: records.length > MAX_SUITE_TEST_ROWS || undefined
    };
  } catch (error) {
    return { tests_error: toCicdError(error, path).message };
  }
}

async function getSuiteResult(client, resultId) {
  const result = await cicdRequest(client, 'GET', `/api/sn_cicd/testsuite/results/${resultId}`);
  const counts = {
    passed: Number(result.rolledup_test_success_count ?? 0),
    failed: Number(result.rolledup_test_failure_count ?? 0),
    errored: Number(result.rolledup_test_error_count ?? 0),
    skipped: Number(result.rolledup_test_skip_count ?? 0)
  };
  const rows = await getSuiteTestRows(client, resultId);
  return compact({
    result_type: 'suite',
    result_id: resultId,
    test_suite_name: result.test_suite_name,
    test_suite_status: result.test_suite_status,
    duration: result.test_suite_duration,
    counts,
    child_suite_results: Array.isArray(result.child_suite_results) && result.child_suite_results.length
      ? result.child_suite_results
      : undefined,
    ...rows,
    links: compact({
      api: apiUrl(client, `/api/sn_cicd/testsuite/results/${resultId}`),
      record: recordUrl(client, 'sys_atf_test_suite_result', resultId)
    })
  });
}

function getResult(client, resultId, resultType) {
  return resultType === 'suite' ? getSuiteResult(client, resultId) : getTestResult(client, resultId);
}

// ---------------------------------------------------------------------------
// Run orchestration
// ---------------------------------------------------------------------------

function jsonResponse(payload, isError = false) {
  return {
    content: [{ type: 'text', text: JSON.stringify(payload, null, 2) }],
    isError
  };
}

function escapeEncodedQueryValue(value) {
  return value.replace(/\^/g, '^^');
}

async function resolveTestSysId(client, testName) {
  const path = '/api/now/table/sys_atf_test';
  let records;
  try {
    const response = await httpClient(client).get(withQuery(path, {
      sysparm_query: `name=${escapeEncodedQueryValue(testName)}`,
      sysparm_fields: 'sys_id,name',
      sysparm_limit: 2
    }));
    records = Array.isArray(response?.data?.result) ? response.data.result : [];
  } catch (error) {
    throw new Error(`Failed to resolve ATF test name '${truncate(testName, 100)}': ${toCicdError(error, path).message}`);
  }
  if (records.length === 0) throw new Error(`No ATF test found with name '${truncate(testName, 100)}'`);
  if (records.length > 1) throw new Error(`Multiple ATF tests found with name '${truncate(testName, 100)}'; use test_sys_id instead`);
  const sysId = records[0]?.sys_id;
  if (typeof sysId !== 'string' || !sysId) throw new Error(`ATF test '${truncate(testName, 100)}' did not return a sys_id`);
  return sysId;
}

async function dispatchAndMaybeWait(client, { path, query, resultType, label, target }, options, deps) {
  const dispatchResult = await cicdRequest(client, 'POST', path, query);
  const progressId = linkOf(dispatchResult?.links, 'progress')?.id;

  if (!options.wait) {
    return jsonResponse(compact({
      status: 'started',
      target,
      progress_id: progressId,
      status_message: dispatchResult?.status_message,
      next_step: progressId
        ? `Call SN-Get-CICD-Progress with progress_id ${progressId} (wait: true to block), then SN-Get-ATF-Results with result_type "${resultType}".`
        : undefined
    }), !progressId);
  }

  if (!progressId) {
    throw new CicdApiError(`sn_cicd request to ${path} returned no progress id; the ${label} may not have started`, { code: 'CICD_NO_PROGRESS_ID' });
  }

  let polled;
  try {
    polled = await pollProgress(client, progressId, options, deps);
  } catch (error) {
    return jsonResponse({
      status: 'unknown',
      target,
      progress_id: progressId,
      message: `The ${label} was started but its progress could not be read: ${toCicdError(error, `/api/sn_cicd/progress/${progressId}`).message}`,
      next_step: `Retry SN-Get-CICD-Progress with progress_id ${progressId}. Do not re-run the ${label} unless the tracker shows it never ran.`
    }, true);
  }

  const { snapshot, timedOut } = polled;
  if (timedOut) {
    return jsonResponse({
      status: 'timeout',
      target,
      progress_id: progressId,
      progress: snapshot,
      message: `Stopped waiting after ${options.timeoutMs / 1000}s; the ${label} is still ${snapshot.state} on the instance. This is not a result.`,
      next_step: `Call SN-Get-CICD-Progress with progress_id ${progressId} and wait: true, then SN-Get-ATF-Results with result_type "${resultType}" and the returned results.id.`
    });
  }

  const successful = snapshot.state === 'successful';
  const payload = { status: snapshot.state, successful, target, progress_id: progressId, progress: snapshot };
  // The tracker state is authoritative for pass/fail; results only add detail.
  if (snapshot.results?.id) {
    try {
      payload.results = await getResult(client, snapshot.results.id, resultType);
    } catch (error) {
      payload.results_error = toCicdError(error, 'results').message;
    }
  }
  return jsonResponse(payload, !successful);
}

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

async function runAtfTest(client, args, deps) {
  assertExactlyOneOf(args, 'test_sys_id', 'test_name');
  const runInCloud = optionalBoolean(args, 'run_in_cloud');
  const captureNodeLogs = optionalBoolean(args, 'capture_node_logs');
  const options = waitOptions(args, true);
  let testSysId;
  if (present(args.test_sys_id)) {
    assertSysId(args.test_sys_id, 'test_sys_id');
    testSysId = args.test_sys_id;
  } else {
    assertName(args.test_name, 'test_name');
    testSysId = await resolveTestSysId(client, args.test_name);
  }
  return dispatchAndMaybeWait(client, {
    path: '/api/sn_cicd/tests/run_test',
    query: { test_sys_id: testSysId, run_in_cloud: runInCloud, capture_node_logs: captureNodeLogs },
    resultType: 'test',
    label: 'ATF test',
    target: compact({ test_sys_id: testSysId, test_name: args.test_name || undefined })
  }, options, deps);
}

async function runAtfSuite(client, args, deps) {
  assertExactlyOneOf(args, 'test_suite_sys_id', 'test_suite_name');
  if (present(args.test_suite_sys_id)) assertSysId(args.test_suite_sys_id, 'test_suite_sys_id');
  else assertName(args.test_suite_name, 'test_suite_name');
  const browserName = optionalString(args, 'browser_name');
  if (browserName !== undefined && !BROWSER_NAMES.includes(browserName)) {
    throw new Error(`browser_name must be one of: ${BROWSER_NAMES.join(', ')}`);
  }
  const query = {
    test_suite_sys_id: present(args.test_suite_sys_id) ? args.test_suite_sys_id : undefined,
    test_suite_name: present(args.test_suite_name) ? args.test_suite_name : undefined,
    browser_name: browserName,
    browser_version: optionalString(args, 'browser_version'),
    os_name: optionalString(args, 'os_name'),
    os_version: optionalString(args, 'os_version'),
    run_in_cloud: optionalBoolean(args, 'run_in_cloud'),
    is_performance_run: optionalBoolean(args, 'is_performance_run')
  };
  const options = waitOptions(args, true);
  return dispatchAndMaybeWait(client, {
    path: '/api/sn_cicd/testsuite/run',
    query,
    resultType: 'suite',
    label: 'ATF test suite',
    target: compact({ test_suite_sys_id: query.test_suite_sys_id, test_suite_name: query.test_suite_name })
  }, options, deps);
}

async function getCicdProgress(client, args, deps) {
  assertSysId(args.progress_id, 'progress_id');
  const options = waitOptions(args, false);
  if (!options.wait) {
    return jsonResponse(await fetchProgress(client, args.progress_id));
  }
  const { snapshot, timedOut } = await pollProgress(client, args.progress_id, options, deps);
  return jsonResponse(timedOut
    ? { ...snapshot, timed_out: true, message: `Still ${snapshot.state} after ${options.timeoutMs / 1000}s; call again to keep waiting.` }
    : snapshot);
}

async function getAtfResults(client, args) {
  assertSysId(args.result_id, 'result_id');
  if (!RESULT_TYPES.includes(args.result_type)) {
    throw new Error(`result_type must be one of: ${RESULT_TYPES.join(', ')}`);
  }
  return jsonResponse(await getResult(client, args.result_id, args.result_type));
}

async function getAtfTestLogs(client, args) {
  assertSysId(args.result_id, 'result_id');
  const pattern = validateLogPattern(args.pattern);
  const limit = args.limit ?? DEFAULT_LOG_LIMIT;
  if (!Number.isInteger(limit) || limit < 1 || limit > MAX_LOG_LIMIT) {
    throw new Error(`limit must be an integer from 1 to ${MAX_LOG_LIMIT}`);
  }
  let result;
  try {
    result = await cicdRequest(client, 'GET', `/api/sn_cicd/tests/test/results/${args.result_id}/logs`, {
      sysparm_regex: pattern,
      sysparm_limit: limit
    });
  } catch (error) {
    if (error?.status === 404 && NO_LOGS_PATTERN.test(error.detail || '')) {
      return jsonResponse(compact({
        result_id: args.result_id,
        pattern,
        limit,
        logs: [],
        note: 'No logs were captured for this test result. Logs are captured only when the run used capture_node_logs: true.'
      }));
    }
    throw error;
  }
  const serialized = JSON.stringify(result);
  const payload = compact({ result_id: args.result_id, pattern, limit });
  if (serialized.length > MAX_LOG_PAYLOAD_CHARS) {
    payload.logs_truncated = true;
    payload.logs_text = `${serialized.slice(0, MAX_LOG_PAYLOAD_CHARS)}…`;
  } else {
    payload.logs = result;
  }
  return jsonResponse(payload);
}

/**
 * @param {string} name - one of ATF_TOOL_NAMES
 * @param {object} args - tool arguments
 * @param {object} client - per-request ServiceNowClient (instance-routed)
 * @param {{sleep?: (ms:number)=>Promise<void>, now?: ()=>number}} [deps]
 */
export async function handleAtfTool(name, args = {}, client, deps = {}) {
  const input = args || {};
  switch (name) {
    case 'SN-Run-ATF-Test':
      return runAtfTest(client, input, deps);
    case 'SN-Run-ATF-Suite':
      return runAtfSuite(client, input, deps);
    case 'SN-Get-CICD-Progress':
      return getCicdProgress(client, input, deps);
    case 'SN-Get-ATF-Results':
      return getAtfResults(client, input);
    case 'SN-Get-ATF-Test-Logs':
      return getAtfTestLogs(client, input);
    default:
      throw new Error(`Unknown ATF tool: ${name}`);
  }
}
