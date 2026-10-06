import { EventEmitter } from 'node:events';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, jest, test, afterEach } from '@jest/globals';
import * as verifier from '../scripts/verify-package.mjs';
import {
  closeListener,
  createVerifierCleanupTasks,
  initializeResponseMaxBytes,
  listenOnLoopback,
  normalizeThrownValue,
  postInitialize,
  run,
  runWithCleanup,
  subprocessDiagnosticMaxBytes,
  subprocessMaxBufferBytes
} from '../scripts/verify-package.mjs';

afterEach(() => {
  jest.useRealTimers();
});
describe('published artifact boundaries', () => {
  const entry = { filename: 'happy-platform-mcp-5.2.2.tgz', files: [{ path: 'package.json' }] };
  test.each([[entry], { 'happy-platform-mcp': entry }].map((output) => ({ output })))('accepts a single npm pack record in either output shape', ({ output }) => {
    expect(verifier.parsePackOutput(JSON.stringify(output))).toEqual(entry);
  });
  test.each([null, [], {}, [entry, entry], { a: entry, b: entry }, [{ ...entry, filename: '../escape.tgz' }], [{ ...entry, files: null }]].map((output) => ({ output })))('rejects ambiguous or unsafe npm pack output %#', ({ output }) => {
    expect(() => verifier.parsePackOutput(JSON.stringify(output))).toThrow(/npm pack/);
  });
  test('accepts runtime source and bundled dependency files', () => {
    expect(() => verifier.assertPackageFiles([{ path: 'src/server.js' }, { path: 'node_modules/@modelcontextprotocol/sdk/dist/esm/server/index.js' }])).not.toThrow();
  });
  test.each(['src/.env', 'src/secret.pem', 'src/config/instance-config.json', 'config/servicenow-instances.json', 'setup/setup-report.json', 'docs/plans/private.md', 'scripts/bundle-sdk-manifest.mjs', 'src/server.js.backup', 'node_modules/.cache/x', '../src/server.js', 'src/../secret.js', 'src\\server.js', 'node_modules/sdk/.env', 'node_modules/sdk/private.key'])('rejects disallowed packed file %s', (path) => {
    expect(() => verifier.assertPackageFiles([{ path }])).toThrow(/packed tarball/);
  });
  test('rejects a dependency outside the SDK bundle closure', () => {
    expect(() => verifier.assertPackageFiles([{ path: 'node_modules/unexpected/index.js' }])).toThrow(/packed tarball/);
  });
  test('rejects an unlisted nested dependency under the SDK directory', () => {
    expect(() => verifier.assertPackageFiles([{ path: 'node_modules/@modelcontextprotocol/sdk/node_modules/unexpected/index.js' }])).toThrow(/packed tarball/);
  });
});

describe('package verifier subprocess diagnostics', () => {
  test('reports nonzero status, signal state, stdout, and stderr', () => {
    expect(() => run(
      process.execPath,
      ['-e', 'process.stdout.write("verifier-out"); process.stderr.write("verifier-err"); process.exit(7)'],
      tmpdir(),
      2_000
    )).toThrow(/failed \(status 7, signal none\)[\s\S]*verifier-out[\s\S]*verifier-err/);
  });

  test('reports timeout, kill signal, and termination status', () => {
    expect(() => run(
      process.execPath,
      ['-e', 'setInterval(() => {}, 1_000)'],
      tmpdir(),
      25
    )).toThrow(/timed out after 25 ms and was sent SIGKILL \(status null, signal (?:SIGKILL|none)\)/);
  });

  test('classifies output overflow and retains bounded partial diagnostics', () => {
    const marker = 'PRIMARY-ENOBUFS-MARKER';
    let caught;

    try {
      run(
        process.execPath,
        ['-e', `process.stdout.write('x'.repeat(100_000) + '${marker}' + 'y'.repeat(100_000))`],
        tmpdir(),
        2_000,
        128 * 1_024
      );
    } catch (error) {
      caught = error;
    }

    expect(caught).toBeInstanceOf(Error);
    expect(caught.cause?.code).toBe('ENOBUFS');
    expect(caught.message).toMatch(
      /exceeded subprocess output limit of 131072 bytes \(status null, signal (?:SIGKILL|SIGTERM|none)\)/
    );
    expect(caught.message).toContain(marker);
    expect(Buffer.byteLength(caught.message)).toBeLessThanOrEqual(subprocessDiagnosticMaxBytes + 1_024);
  });

  test('preserves bounded partial diagnostics and cause for subprocess timeout', () => {
    const marker = 'PRIMARY-TIMEOUT-MARKER';
    let caught;

    try {
      run(
        process.execPath,
        ['-e', `process.stderr.write('${marker}' + 'y'.repeat(100_000)); setInterval(() => {}, 1_000)`],
        tmpdir(),
        25
      );
    } catch (error) {
      caught = error;
    }

    expect(caught).toBeInstanceOf(Error);
    expect(caught.cause?.code).toBe('ETIMEDOUT');
    expect(caught.message).toContain('timed out after 25 ms');
    expect(caught.message).toContain(marker);
    expect(Buffer.byteLength(caught.message)).toBeLessThanOrEqual(subprocessDiagnosticMaxBytes + 1_024);
  });

  test('preserves startup cause when a subprocess cannot be started', () => {
    let caught;

    try {
      run(
        join(tmpdir(), 'missing-package-verifier-command'),
        [],
        tmpdir(),
        2_000
      );
    } catch (error) {
      caught = error;
    }

    expect(caught).toBeInstanceOf(Error);
    expect(caught.cause?.code).toBe('ENOENT');
    expect(caught.message).toMatch(/could not be started \(status null, signal none\)/);
  });

  test('exports explicit subprocess output and diagnostic limits', () => {
    expect(subprocessMaxBufferBytes).toBe(16 * 1_024 * 1_024);
    expect(subprocessDiagnosticMaxBytes).toBe(64 * 1_024);
  });
});

describe('package verifier listener lifecycle', () => {
  test('removes startup listeners after an error', async () => {
    const listener = new EventEmitter();
    listener.listen = jest.fn(() => queueMicrotask(() => listener.emit('error', new Error('bind failed'))));

    await expect(listenOnLoopback(listener, 100)).rejects.toThrow('bind failed');
    expect(listener.listenerCount('error')).toBe(0);
    expect(listener.listenerCount('listening')).toBe(0);
  });

  test('times out startup and removes both listeners', async () => {
    jest.useFakeTimers();
    const listener = new EventEmitter();
    listener.listen = jest.fn();

    const pending = expect(listenOnLoopback(listener, 25))
      .rejects.toThrow('startup timed out after 25 ms');
    await jest.advanceTimersByTimeAsync(25);
    await pending;
    expect(listener.listenerCount('error')).toBe(0);
    expect(listener.listenerCount('listening')).toBe(0);
    expect(jest.getTimerCount()).toBe(0);
  });

  test('removes close error listener after callback error', async () => {
    const listener = new EventEmitter();
    listener.closeAllConnections = jest.fn();
    listener.close = jest.fn((callback) => queueMicrotask(() => callback(new Error('close failed'))));

    await expect(closeListener(listener, 100)).rejects.toThrow('close failed');
    expect(listener.listenerCount('error')).toBe(0);
  });

  test('forces connections closed on close timeout and removes error listener', async () => {
    jest.useFakeTimers();
    const listener = new EventEmitter();
    listener.closeAllConnections = jest.fn();
    listener.close = jest.fn();

    const pending = expect(closeListener(listener, 25))
      .rejects.toThrow('close timed out after 25 ms');
    await jest.advanceTimersByTimeAsync(25);
    await pending;
    expect(listener.closeAllConnections).toHaveBeenCalledTimes(2);
    expect(listener.listenerCount('error')).toBe(0);
    expect(jest.getTimerCount()).toBe(0);
  });
});

describe('package verifier initialize request lifecycle', () => {
  function createRequestHarness(onEnd) {
    const request = new EventEmitter();
    const response = new EventEmitter();
    response.headers = { 'content-type': 'application/json' };
    response.statusCode = 200;
    response.destroy = jest.fn();
    request.destroy = jest.fn();
    request.end = jest.fn(() => onEnd(request, response));
    const requestImplementation = jest.fn((options, onResponse) => {
      request.options = options;
      request.onResponse = onResponse;
      return request;
    });
    return { request, requestImplementation, response };
  }

  test('accepts an initialize response exactly at the configured byte limit', async () => {
    const body = '{"ok":true}';
    const harness = createRequestHarness((_request, response) => {
      harness.request.onResponse(response);
      response.emit('data', Buffer.from(body));
      response.emit('end');
    });

    await expect(postInitialize(1234, {
      maxResponseBytes: Buffer.byteLength(body),
      requestImplementation: harness.requestImplementation,
      timeoutMs: 100
    })).resolves.toMatchObject({ body });
    expect(harness.request.destroy).not.toHaveBeenCalled();
    expect(harness.response.destroy).not.toHaveBeenCalled();
  });

  test('rejects an initialize response one byte over the configured limit', async () => {
    jest.useFakeTimers();
    const harness = createRequestHarness((_request, response) => {
      harness.request.onResponse(response);
      response.emit('data', Buffer.alloc(9));
    });

    const pending = expect(postInitialize(1234, {
      maxResponseBytes: 8,
      requestImplementation: harness.requestImplementation,
      timeoutMs: 100
    })).rejects.toMatchObject({
      allowedBytes: 8,
      observedBytes: 9,
      retainedBytes: 0,
      message: 'initialize response exceeded maximum size: observed 9 bytes, allowed 8 bytes'
    });
    await jest.advanceTimersByTimeAsync(0);
    await pending;
    expect(harness.request.options.signal.aborted).toBe(true);
    expect(harness.request.destroy).toHaveBeenCalledTimes(1);
    expect(harness.response.destroy).toHaveBeenCalledTimes(1);
    expect(harness.request.listenerCount('error')).toBe(0);
    expect(harness.response.eventNames()).toEqual([]);
    expect(jest.getTimerCount()).toBe(0);
  });

  test('caps retained initialize bytes during rapid multi-chunk overflow and cleans up once', async () => {
    jest.useFakeTimers();
    const harness = createRequestHarness((_request, response) => {
      harness.request.onResponse(response);
      response.emit('data', Buffer.alloc(4));
      response.emit('data', Buffer.alloc(4));
      response.emit('data', Buffer.alloc(1));
      response.emit('data', Buffer.alloc(1_000_000));
    });
    harness.response.destroy.mockImplementation(() => {
      harness.response.emit('error', new Error('response destroy side effect'));
      harness.response.emit('close');
    });
    harness.request.destroy.mockImplementation(() => {
      harness.request.emit('error', new Error('request destroy side effect'));
    });

    const pending = expect(postInitialize(1234, {
      maxResponseBytes: 8,
      requestImplementation: harness.requestImplementation,
      timeoutMs: 100
    })).rejects.toMatchObject({
      allowedBytes: 8,
      observedBytes: 9,
      retainedBytes: 8,
      message: 'initialize response exceeded maximum size: observed 9 bytes, allowed 8 bytes'
    });
    await jest.advanceTimersByTimeAsync(0);
    await pending;
    expect(harness.request.destroy).toHaveBeenCalledTimes(1);
    expect(harness.response.destroy).toHaveBeenCalledTimes(1);
    expect(harness.request.listenerCount('error')).toBe(0);
    expect(harness.response.eventNames()).toEqual([]);
    expect(jest.getTimerCount()).toBe(0);
  });

  test('preserves size overflow before synchronous teardown failures', async () => {
    jest.useFakeTimers();
    const responseCleanupError = new Error('response cleanup failed');
    const requestCleanupError = new Error('request cleanup failed');
    const harness = createRequestHarness((_request, response) => {
      harness.request.onResponse(response);
      response.emit('data', Buffer.alloc(9));
    });
    harness.response.destroy.mockImplementation(() => {
      throw responseCleanupError;
    });
    harness.request.destroy.mockImplementation(() => {
      throw requestCleanupError;
    });

    const rejection = postInitialize(1234, {
      maxResponseBytes: 8,
      requestImplementation: harness.requestImplementation,
      timeoutMs: 100
    }).catch((error) => error);
    await jest.advanceTimersByTimeAsync(0);
    const error = await rejection;

    expect(error).toBeInstanceOf(AggregateError);
    expect(error.errors).toHaveLength(3);
    expect(error.errors[0]).toMatchObject({
      allowedBytes: 8,
      observedBytes: 9,
      retainedBytes: 0,
      message: 'initialize response exceeded maximum size: observed 9 bytes, allowed 8 bytes'
    });
    expect(error.errors.slice(1)).toEqual([responseCleanupError, requestCleanupError]);
    expect(harness.request.listenerCount('error')).toBe(0);
    expect(harness.response.eventNames()).toEqual([]);
    expect(jest.getTimerCount()).toBe(0);
  });

  test.each([0, -1, 1.5, Number.POSITIVE_INFINITY])(
    'rejects invalid initialize response byte limit %p before starting I/O',
    (maxResponseBytes) => {
      const requestImplementation = jest.fn();

      expect(() => postInitialize(1234, {
        maxResponseBytes,
        requestImplementation,
        timeoutMs: 100
      })).toThrow('initialize response byte limit must be a positive safe integer');
      expect(requestImplementation).not.toHaveBeenCalled();
    }
  );

  test('exports a small explicit default initialize response limit', () => {
    expect(initializeResponseMaxBytes).toBe(64 * 1_024);
  });

  test('rejects an aborted response and removes request and response listeners', async () => {
    jest.useFakeTimers();
    const harness = createRequestHarness((_request, response) => {
      harness.request.onResponse(response);
      response.emit('aborted');
    });

    await expect(postInitialize(1234, {
      requestImplementation: harness.requestImplementation,
      timeoutMs: 100
    })).rejects.toThrow('response was aborted');

    expect(harness.request.listenerCount('error')).toBe(0);
    expect(harness.response.listenerCount('data')).toBe(0);
    expect(harness.response.listenerCount('error')).toBe(0);
    expect(harness.response.listenerCount('aborted')).toBe(0);
    expect(harness.response.listenerCount('close')).toBe(0);
    expect(harness.response.listenerCount('end')).toBe(0);
    expect(jest.getTimerCount()).toBe(0);
  });

  test('preserves a genuine response error before the initialize deadline', async () => {
    jest.useFakeTimers();
    const responseError = new Error('response stream failed');
    const harness = createRequestHarness((_request, response) => {
      harness.request.onResponse(response);
      response.emit('error', responseError);
    });

    await expect(postInitialize(1234, {
      requestImplementation: harness.requestImplementation,
      timeoutMs: 100
    })).rejects.toBe(responseError);

    expect(harness.request.listenerCount('error')).toBe(0);
    expect(harness.response.listenerCount('data')).toBe(0);
    expect(harness.response.listenerCount('error')).toBe(0);
    expect(harness.response.listenerCount('aborted')).toBe(0);
    expect(harness.response.listenerCount('close')).toBe(0);
    expect(harness.response.listenerCount('end')).toBe(0);
    expect(jest.getTimerCount()).toBe(0);
  });

  test('preserves the initialize timeout for a real partial response that never ends', async () => {
    let markPartialResponseWritten;
    const partialResponseWritten = new Promise((resolve) => {
      markPartialResponseWritten = resolve;
    });
    const server = createServer((_request, response) => {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.write('{"jsonrpc":"2.0"');
      markPartialResponseWritten();
    });

    await new Promise((resolve, reject) => {
      server.once('error', reject);
      server.listen(0, '127.0.0.1', resolve);
    });

    try {
      const { port } = server.address();
      const pending = expect(postInitialize(port, {
        timeoutMs: 100
      })).rejects.toThrow(
        'initialize request timed out after 100 ms before the complete response body was received'
      );

      await partialResponseWritten;
      await pending;
    } finally {
      await new Promise((resolve, reject) => {
        server.close((error) => error ? reject(error) : resolve());
        server.closeAllConnections();
      });
    }
  });

  test.each([
    ['response error', (response) => response.emit('error', new Error('teardown response error'))],
    ['response abort', (response) => response.emit('aborted')],
    ['response close', (response) => response.emit('close')],
    ['response end', (response) => response.emit('end')]
  ])('preserves the initialize timeout when teardown emits %s', async (_event, emitTerminalEvent) => {
    jest.useFakeTimers();
    const harness = createRequestHarness((_request, response) => {
      harness.request.onResponse(response);
      response.emit('data', Buffer.from('{"jsonrpc":"2.0"'));
    });
    harness.response.destroy.mockImplementation(() => emitTerminalEvent(harness.response));

    const pending = expect(postInitialize(1234, {
      requestImplementation: harness.requestImplementation,
      timeoutMs: 25
    })).rejects.toThrow('timed out after 25 ms before the complete response body was received');
    await jest.advanceTimersByTimeAsync(25);
    await pending;

    expect(harness.request.listenerCount('error')).toBe(0);
    expect(harness.response.listenerCount('data')).toBe(0);
    expect(harness.response.listenerCount('error')).toBe(0);
    expect(harness.response.listenerCount('aborted')).toBe(0);
    expect(harness.response.listenerCount('close')).toBe(0);
    expect(harness.response.listenerCount('end')).toBe(0);
    expect(jest.getTimerCount()).toBe(0);
  });

  test('enforces a full-response deadline, aborts I/O, and removes listeners', async () => {
    jest.useFakeTimers();
    const harness = createRequestHarness((_request, response) => {
      harness.request.onResponse(response);
      response.emit('data', Buffer.from('{"jsonrpc":"2.0"'));
    });
    harness.request.destroy.mockImplementation(() => {
      queueMicrotask(() => harness.request.emit('error', new Error('abort side effect')));
    });

    const pending = expect(postInitialize(1234, {
      requestImplementation: harness.requestImplementation,
      timeoutMs: 25
    })).rejects.toThrow('timed out after 25 ms before the complete response body was received');
    await jest.advanceTimersByTimeAsync(25);
    await pending;
    expect(harness.request.options.signal.aborted).toBe(true);
    expect(harness.request.destroy).toHaveBeenCalledTimes(1);
    expect(harness.response.destroy).toHaveBeenCalledTimes(1);
    expect(harness.request.listenerCount('error')).toBe(0);
    expect(harness.response.listenerCount('data')).toBe(0);
    expect(harness.response.listenerCount('error')).toBe(0);
    expect(harness.response.listenerCount('aborted')).toBe(0);
    expect(harness.response.listenerCount('close')).toBe(0);
    expect(harness.response.listenerCount('end')).toBe(0);
    expect(jest.getTimerCount()).toBe(0);
  });
});

describe('package verifier error normalization and cleanup orchestration', () => {
  test.each([
    [undefined, 'undefined'],
    [null, 'null'],
    [false, 'false'],
    [0, '0'],
    ['', "''"]
  ])('normalizes falsy non-Error value %p', (value, rendered) => {
    const error = normalizeThrownValue(value, 'probe');
    expect(error).toBeInstanceOf(Error);
    expect(error.message).toContain(rendered);
    expect(error.cause).toBe(value);
  });

  test('returns the primary result when cleanup succeeds', async () => {
    await expect(runWithCleanup(
      async () => 'verified',
      [async () => undefined]
    )).resolves.toBe('verified');
  });

  test('throws cleanup-only failures', async () => {
    const cleanupError = new Error('cleanup only');
    const caught = await runWithCleanup(
      async () => 'verified',
      [async () => { throw cleanupError; }]
    ).catch((error) => error);

    expect(caught).toBeInstanceOf(AggregateError);
    expect(caught.errors).toEqual([cleanupError]);
  });

  test('preserves primary failure before every cleanup failure', async () => {
    const primaryError = new Error('primary');
    const firstCleanupError = new Error('first cleanup');
    const secondCleanupError = new Error('second cleanup');
    const calls = [];
    const caught = await runWithCleanup(
      async () => { throw primaryError; },
      [
        async () => { calls.push('first'); throw firstCleanupError; },
        async () => { calls.push('second'); throw secondCleanupError; }
      ]
    ).catch((error) => error);

    expect(calls).toEqual(['first', 'second']);
    expect(caught).toBeInstanceOf(AggregateError);
    expect(caught.errors).toEqual([primaryError, firstCleanupError, secondCleanupError]);
  });

  test('normalizes falsy primary and cleanup rejections in order', async () => {
    const caught = await runWithCleanup(
      async () => { throw 0; },
      [async () => { throw false; }]
    ).catch((error) => error);

    expect(caught).toBeInstanceOf(AggregateError);
    expect(caught.errors).toHaveLength(2);
    expect(caught.errors[0].message).toContain('non-Error value: 0');
    expect(caught.errors[0].cause).toBe(0);
    expect(caught.errors[1].message).toContain('non-Error value: false');
    expect(caught.errors[1].cause).toBe(false);
  });

  test('partial initialization cleans every acquired resource and aggregates workspace removal failure', async () => {
    const calls = [];
    const transportError = new Error('transport close failed');
    const workspaceError = new Error('workspace removal failed');
    const resources = {
      transport: { close: jest.fn(async () => { calls.push('transport'); throw transportError; }) },
      server: { close: jest.fn(async () => { calls.push('server'); }) },
      listener: { id: 'listener' },
      workspace: '/tmp/package-verifier-partial'
    };
    const closeListenerImplementation = jest.fn(async () => { calls.push('listener'); });
    const removeWorkspace = jest.fn(async () => { calls.push('workspace'); throw workspaceError; });
    const primaryError = new Error('initialize failed');

    const caught = await runWithCleanup(
      async () => { throw primaryError; },
      createVerifierCleanupTasks(resources, {
        closeListenerImplementation,
        removeWorkspace,
        resourceTimeoutMs: 100,
        workspaceTimeoutMs: 100
      })
    ).catch((error) => error);

    expect(calls).toEqual(['transport', 'server', 'listener', 'workspace']);
    expect(closeListenerImplementation).toHaveBeenCalledWith(resources.listener, 100);
    expect(removeWorkspace).toHaveBeenCalledWith(resources.workspace, { recursive: true, force: true });
    expect(caught).toBeInstanceOf(AggregateError);
    expect(caught.errors).toEqual([primaryError, transportError, workspaceError]);
  });
});
