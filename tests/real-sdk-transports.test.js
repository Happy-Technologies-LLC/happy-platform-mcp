import { describe, expect, jest, test } from '@jest/globals';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { SSEClientTransport } from '@modelcontextprotocol/sdk/client/sse.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { randomBytes } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { EventEmitter } from 'node:events';
import { readFileSync } from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { createHttpApp } from '../src/http-server.js';

const projectRoot = fileURLToPath(new URL('..', import.meta.url));
const stdioServerPath = path.join(projectRoot, 'src', 'stdio-server.js');
const packageJson = JSON.parse(readFileSync(path.join(projectRoot, 'package.json'), 'utf8'));
const expectedServerIdentity = { name: packageJson.name, version: packageJson.version };
const TEST_DEADLINE_MS = 8_000;
const MCP_OPERATION_TIMEOUT_MS = 4_000;
const CLEANUP_TIMEOUT_MS = 1_000;
const FAILURE_PROBE_BUDGET_MS = 250;
const FAILURE_PROBE_TIMEOUT_MS = 25;

function createSdkClient(name) {
  return new Client({ name, version: '1.0.0' });
}

async function withTimeout(operation, label, timeoutMs) {
  let timer;
  const operationPromise = Promise.resolve().then(operation);
  const timeoutPromise = new Promise((_, reject) => {
    timer = setTimeout(
      () => reject(new Error(`${label} timed out after ${timeoutMs}ms`)),
      timeoutMs
    );
    timer.unref?.();
  });

  try {
    return await Promise.race([operationPromise, timeoutPromise]);
  } finally {
    clearTimeout(timer);
  }
}

function createDeadline(scope, budgetMs = TEST_DEADLINE_MS) {
  const expiresAt = performance.now() + budgetMs;

  return (operation, label, timeoutMs = MCP_OPERATION_TIMEOUT_MS) => {
    const remainingMs = Math.max(1, Math.ceil(expiresAt - performance.now()));
    return withTimeout(operation, `${scope}: ${label}`, Math.min(timeoutMs, remainingMs));
  };
}

function listenOnEphemeralLoopback(app) {
  const server = app.listen(0, '127.0.0.1');
  const listening = new Promise((resolve, reject) => {
    function onListening() {
      server.removeListener('error', onError);
      resolve();
    }

    function onError(error) {
      server.removeListener('listening', onListening);
      reject(error);
    }

    server.once('listening', onListening);
    server.once('error', onError);
  });
  return { server, listening };
}

async function closeHttpServer(server) {
  await new Promise((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
    server.closeAllConnections?.();
  });
}

async function attemptCleanup(deadline, errors, label, operation, timeoutMs) {
  try {
    await deadline(operation, label, timeoutMs);
  } catch (error) {
    errors.push(error);
  }
}

async function forceTerminateStdioChild(child, deadline, timeoutMs) {
  if (
    !child
    || typeof child.kill !== 'function'
    || child.exitCode !== null
    || child.signalCode !== null
  ) {
    return;
  }

  let resolveClose;
  let rejectClose;
  const closeConfirmed = new Promise((resolve, reject) => {
    resolveClose = resolve;
    rejectClose = reject;
  });
  const onClose = () => resolveClose();
  const onError = (error) => rejectClose(error);
  child.once('close', onClose);
  child.once('error', onError);

  try {
    if (!child.kill('SIGKILL')) {
      throw new Error('Failed to forcibly terminate the stdio child process');
    }
    await deadline(
      () => closeConfirmed,
      'stdio child SIGKILL close confirmation',
      timeoutMs
    );
  } finally {
    child.removeListener('close', onClose);
    child.removeListener('error', onError);
  }
}

async function runWithCleanup(operation, cleanup) {
  let result;
  let primaryError;
  let cleanupError;
  let operationFailed = false;
  let cleanupFailed = false;

  try {
    result = await operation();
  } catch (error) {
    operationFailed = true;
    primaryError = error;
  }

  try {
    await cleanup();
  } catch (error) {
    cleanupFailed = true;
    cleanupError = error;
  }

  if (operationFailed && cleanupFailed) {
    throw new AggregateError(
      [primaryError, cleanupError],
      'MCP operation and cleanup failed'
    );
  }
  if (operationFailed) {
    throw primaryError;
  }
  if (cleanupFailed) {
    throw cleanupError;
  }
  return result;
}

async function closeSdkResources({
  client,
  transport,
  server,
  deadline,
  cleanupTimeoutMs = CLEANUP_TIMEOUT_MS
}) {
  const errors = [];
  const stdioChild = transport?._process;

  if (client) {
    await attemptCleanup(
      deadline,
      errors,
      'client close',
      () => client.close(),
      cleanupTimeoutMs
    );
  }
  if (transport) {
    await attemptCleanup(
      deadline,
      errors,
      'transport close',
      () => transport.close(),
      cleanupTimeoutMs
    );
  }
  if (server) {
    await attemptCleanup(
      deadline,
      errors,
      'HTTP listener close',
      () => closeHttpServer(server),
      cleanupTimeoutMs
    );
  }
  if (
    stdioChild
    && stdioChild.exitCode === null
    && stdioChild.signalCode === null
  ) {
    try {
      await forceTerminateStdioChild(stdioChild, deadline, cleanupTimeoutMs);
    } catch (error) {
      errors.push(error);
    }
  }

  if (errors.length > 0) {
    throw new AggregateError(errors, 'MCP cleanup failed');
  }
}

describe('real SDK production transports', () => {
  test('removes the startup error listener after HTTP listener startup', async () => {
    const server = new EventEmitter();
    const app = {
      listen: jest.fn(() => server)
    };
    const listener = listenOnEphemeralLoopback(app);

    expect(server.listenerCount('listening')).toBe(1);
    expect(server.listenerCount('error')).toBe(1);
    server.emit('listening');
    await listener.listening;

    expect(app.listen).toHaveBeenCalledWith(0, '127.0.0.1');
    expect(server.listenerCount('listening')).toBe(0);
    expect(server.listenerCount('error')).toBe(0);
    const laterError = new Error('post-startup listener failure');
    expect(() => server.emit('error', laterError)).toThrow();
  });

  test('removes the startup listening handler after HTTP listener failure', async () => {
    const server = new EventEmitter();
    const app = {
      listen: jest.fn(() => server)
    };
    const listener = listenOnEphemeralLoopback(app);
    const startupError = new Error('HTTP listener startup failed');
    const startupFailure = listener.listening.catch((error) => error);

    expect(server.listenerCount('listening')).toBe(1);
    expect(server.listenerCount('error')).toBe(1);
    server.emit('error', startupError);

    expect(await startupFailure).toBe(startupError);
    expect(server.listenerCount('listening')).toBe(0);
    expect(server.listenerCount('error')).toBe(0);
  });

  test('bounds stalled operations and attempts every cleanup path', async () => {
    let primaryCleanupAttempted = false;
    const probeDeadline = createDeadline('failure-path probe', FAILURE_PROBE_BUDGET_MS);

    await expect((async () => {
      try {
        await probeDeadline(
          () => new Promise(() => {}),
          'stalled operation',
          FAILURE_PROBE_TIMEOUT_MS
        );
      } finally {
        primaryCleanupAttempted = true;
      }
    })()).rejects.toThrow(
      `failure-path probe: stalled operation timed out after ${FAILURE_PROBE_TIMEOUT_MS}ms`
    );
    expect(primaryCleanupAttempted).toBe(true);

    const cleanupAttempts = [];
    const child = new EventEmitter();
    let resolveKillObserved;
    const killObserved = new Promise((resolve) => {
      resolveKillObserved = resolve;
    });
    child.exitCode = null;
    child.signalCode = null;
    child.kill = jest.fn((signal) => {
      cleanupAttempts.push(`child:${signal}`);
      resolveKillObserved();
      return true;
    });
    const client = {
      close: jest.fn(() => {
        cleanupAttempts.push('client');
        return new Promise(() => {});
      })
    };
    const transport = {
      _process: child,
      close: jest.fn(async () => {
        cleanupAttempts.push('transport');
      })
    };
    const server = {
      close: jest.fn((callback) => {
        cleanupAttempts.push('server');
        callback();
      }),
      closeAllConnections: jest.fn()
    };

    let cleanupSettled = false;
    const cleanup = closeSdkResources({
      client,
      transport,
      server,
      deadline: createDeadline('cleanup probe', FAILURE_PROBE_BUDGET_MS),
      cleanupTimeoutMs: FAILURE_PROBE_TIMEOUT_MS
    });
    cleanup.then(
      () => { cleanupSettled = true; },
      () => { cleanupSettled = true; }
    );

    await probeDeadline(
      () => killObserved,
      'stdio child SIGKILL observation',
      FAILURE_PROBE_TIMEOUT_MS
    );
    await new Promise((resolve) => setImmediate(resolve));
    expect(cleanupSettled).toBe(false);
    child.signalCode = 'SIGKILL';
    child.emit('close', null, 'SIGKILL');
    await expect(cleanup).rejects.toThrow('MCP cleanup failed');
    expect(cleanupAttempts).toEqual(['client', 'transport', 'server', 'child:SIGKILL']);
    expect(server.closeAllConnections).toHaveBeenCalledTimes(1);
    expect(child.listenerCount('close')).toBe(0);
    expect(child.listenerCount('error')).toBe(0);
  });

  test('preserves the primary failure ahead of a cleanup failure', async () => {
    const primaryError = new Error('primary transport failure');
    const cleanupError = new Error('cleanup failure');

    const error = await runWithCleanup(
      async () => { throw primaryError; },
      async () => { throw cleanupError; }
    ).catch((caught) => caught);

    expect(error).toBeInstanceOf(AggregateError);
    expect(error.errors).toEqual([primaryError, cleanupError]);

    const cleanupOnlyError = await runWithCleanup(
      async () => 'connected',
      async () => { throw cleanupError; }
    ).catch((caught) => caught);
    expect(cleanupOnlyError).toBe(cleanupError);
  });

  test('round-trips authenticated HTTP/SSE tools against a fake ServiceNow endpoint', async () => {
    const records = [{ sys_id: 'smoke-record', short_description: 'SDK transport smoke' }];
    const serviceNowRequests = [];
    const fakeServiceNow = http.createServer((req, res) => {
      serviceNowRequests.push({ method: req.method, url: req.url, authorization: req.headers.authorization });
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ result: records }));
    });
    const apiToken = randomBytes(32).toString('hex');
    const deadline = createDeadline('HTTP/SSE smoke');
    let server;
    let transport;
    let client;

    await runWithCleanup(async () => {
      const serviceNowListener = listenOnEphemeralLoopback(fakeServiceNow);
      await deadline(() => serviceNowListener.listening, 'fake ServiceNow startup');
      const defaultInstance = {
        name: 'smoke',
        url: `http://127.0.0.1:${fakeServiceNow.address().port}`,
        authType: 'basic',
        username: 'smoke-user',
        password: 'smoke-password'
      };
      const app = createHttpApp({ apiToken, defaultInstance, credentialStore: {} });
      const listener = listenOnEphemeralLoopback(app);
      server = listener.server;
      await deadline(() => listener.listening, 'HTTP listener startup');
      const { port } = server.address();
      transport = new SSEClientTransport(new URL(`http://127.0.0.1:${port}/mcp`), {
        requestInit: { headers: { Authorization: `Bearer ${apiToken}` } }
      });
      client = createSdkClient('http-sse-smoke');

      await deadline(() => client.connect(transport), 'connect');
      const tools = await deadline(() => client.listTools(), 'listTools');
      const result = await deadline(() => client.callTool({
        name: 'SN-Query-Table',
        arguments: {
          table_name: 'incident',
          query: 'active=true',
          fields: 'sys_id,short_description',
          limit: 1
        }
      }), 'callTool');

      expect(client.getServerVersion()).toEqual(expectedServerIdentity);
      expect(tools.tools.some((tool) => tool.name === 'SN-Query-Table')).toBe(true);
      expect(serviceNowRequests).toEqual([{
        method: 'GET',
        url: '/api/now/table/incident?sysparm_query=active%3Dtrue&sysparm_limit=1&sysparm_fields=sys_id%2Cshort_description',
        authorization: `Basic ${Buffer.from('smoke-user:smoke-password').toString('base64')}`
      }]);
      expect(JSON.stringify(serviceNowRequests)).not.toContain(apiToken);
      expect(result.isError).not.toBe(true);
      expect(result.content).toEqual([{
        type: 'text',
        text: `Found 1 records in incident:\n${JSON.stringify(records, null, 2)}`
      }]);
    }, async () => {
      await closeSdkResources({ client, transport, server, deadline });
      await deadline(() => closeHttpServer(fakeServiceNow), 'fake ServiceNow close', CLEANUP_TIMEOUT_MS);
    });
  });

  test.each([
    ['an unauthenticated client', () => ({}), 401],
    ['a client with the wrong bearer', () => ({ Authorization: `Bearer ${randomBytes(32).toString('hex')}` }), 401],
    ['a foreign-origin client', (apiToken) => ({ Authorization: `Bearer ${apiToken}`, Origin: 'https://evil.example' }), 403]
  ])('denies %s before any SSE session setup', async (_label, headersFor, status) => {
    const apiToken = randomBytes(32).toString('hex');
    const createServiceNowClient = jest.fn(() => {
      throw new Error('denied clients must not reach session setup');
    });
    const app = createHttpApp({
      apiToken,
      defaultInstance: { name: 'smoke', url: 'https://smoke.invalid' },
      credentialStore: {},
      createServiceNowClient
    });
    const deadline = createDeadline('HTTP/SSE denial');
    let server;
    let transport;
    let client;

    await runWithCleanup(async () => {
      const listener = listenOnEphemeralLoopback(app);
      server = listener.server;
      await deadline(() => listener.listening, 'HTTP listener startup');
      transport = new SSEClientTransport(new URL(`http://127.0.0.1:${server.address().port}/mcp`), {
        requestInit: { headers: headersFor(apiToken) }
      });
      client = createSdkClient('http-sse-denied');

      const error = await deadline(() => client.connect(transport).then(
        () => null,
        (caught) => caught
      ), 'denied connect');
      expect(error).toMatchObject({ code: status });
      expect(createServiceNowClient).not.toHaveBeenCalled();
    }, () => closeSdkResources({ client, transport, server, deadline }));
  });

  test('round-trips a deterministic docs-only tool over production stdio', async () => {
    const transport = new StdioClientTransport({
      command: process.execPath,
      args: [stdioServerPath, '--docs-only'],
      cwd: projectRoot,
      env: {
        HAPPY_MCP_DOCS_ONLY: 'true',
        HAPPY_DOCS_ENABLE_LOCAL_INDEX: 'false',
        HAPPY_DOCS_ENABLE_VECTOR: 'false',
        HAPPY_CONFIG_PATH: path.join(projectRoot, 'tests', '__missing-smoke-config.json'),
        SERVICENOW_INSTANCE_URL: '',
        SERVICENOW_USERNAME: '',
        SERVICENOW_PASSWORD: ''
      },
      stderr: 'inherit'
    });
    const client = createSdkClient('stdio-smoke');
    const deadline = createDeadline('stdio smoke');

    await runWithCleanup(async () => {
      await deadline(() => client.connect(transport), 'connect');
      const tools = await deadline(() => client.listTools(), 'listTools');
      const result = await deadline(() => client.callTool({
        name: 'SN-Docs-Status',
        arguments: {}
      }), 'callTool');
      const status = JSON.parse(result.content[0].text);

      expect(client.getServerVersion()).toEqual(expectedServerIdentity);
      expect(tools.tools.some((tool) => tool.name === 'SN-Docs-Status')).toBe(true);
      expect(result.isError).not.toBe(true);
      expect(status).toMatchObject({
        localIndexEnabled: false,
        ftsAvailable: false,
        families: []
      });
    }, () => closeSdkResources({ client, transport, deadline }));
  });
});
