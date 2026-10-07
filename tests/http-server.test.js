import { jest } from '@jest/globals';
import crypto from 'node:crypto';
import http from 'node:http';
import os from 'node:os';
import { Server } from '@modelcontextprotocol/sdk/server/index.js';
import { SSEServerTransport } from '@modelcontextprotocol/sdk/server/sse.js';
import { CallToolRequestSchema } from '@modelcontextprotocol/sdk/types.js';
import {
  DEFAULT_HTTP_LIMITS,
  createAuthFailureLimiter,
  createDefaultClient,
  createHttpApp,
  loadHttpSecurityConfig
} from '../src/http-server.js';

const TOKEN = crypto.randomBytes(32).toString('hex');
const BASE64URL_TOKEN = crypto.randomBytes(32).toString('base64url');
const AUTH = { Authorization: `Bearer ${TOKEN}` };
const DEFAULT_INSTANCE = { name: 'test', url: 'https://example.service-now.com' };
const LAN_ADDRESS = Object.values(os.networkInterfaces()).flat()
  .find((entry) => entry && entry.family === 'IPv4' && !entry.internal)?.address;

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(predicate, label, timeoutMs = 3000) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) {
      throw new Error(`Timed out waiting for ${label}`);
    }
    await sleep(5);
  }
}

async function startApp(app) {
  const server = app.listen(0, '127.0.0.1');
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address();
  return {
    port,
    url: `http://127.0.0.1:${port}`,
    close: async () => {
      await app.closeAllSessions();
      await new Promise((resolve) => {
        server.close(resolve);
        server.closeAllConnections();
      });
    }
  };
}

function request(port, { method = 'GET', path = '/health', headers = AUTH, body, host = '127.0.0.1' } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host, port, method, path, headers, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (chunk) => chunks.push(chunk));
      res.on('end', () => resolve({
        status: res.statusCode,
        headers: res.headers,
        body: Buffer.concat(chunks).toString('utf8')
      }));
      res.on('error', reject);
    });
    req.on('error', reject);
    req.end(body);
  });
}

function postMessage(port, sessionId, message, headers = AUTH) {
  const body = typeof message === 'string' ? message : JSON.stringify(message);
  return request(port, {
    method: 'POST',
    path: `/mcp?sessionId=${encodeURIComponent(sessionId)}`,
    headers: { ...headers, 'Content-Type': 'application/json' },
    body
  });
}

function openSse(port, headers = AUTH) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: '127.0.0.1', port, method: 'GET', path: '/mcp', headers, agent: false });
    req.on('error', (error) => {
      if (error.code !== 'ECONNRESET') reject(error);
    });
    req.on('response', (res) => {
      if (res.statusCode !== 200) {
        const chunks = [];
        res.on('data', (chunk) => chunks.push(chunk));
        res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString('utf8') }));
        return;
      }
      const stream = {
        status: 200,
        events: [],
        keepalives: 0,
        ended: false,
        res,
        close: () => req.destroy()
      };
      let buffer = '';
      let resolved = false;
      res.setEncoding('utf8');
      res.on('data', (chunk) => {
        buffer += chunk;
        let boundary;
        while ((boundary = buffer.indexOf('\n\n')) !== -1) {
          const block = buffer.slice(0, boundary);
          buffer = buffer.slice(boundary + 2);
          if (block === ': keepalive') {
            stream.keepalives += 1;
            continue;
          }
          const event = /^event: (.+)$/m.exec(block)?.[1];
          const data = /^data: (.*)$/m.exec(block)?.[1];
          stream.events.push({ event, data });
          if (event === 'endpoint' && !resolved) {
            resolved = true;
            stream.sessionId = new URL(data, 'http://localhost').searchParams.get('sessionId');
            resolve(stream);
          }
        }
      });
      const markEnded = () => {
        stream.ended = true;
        if (!resolved) {
          resolved = true;
          resolve(stream);
        }
      };
      res.on('end', markEnded);
      res.on('close', markEnded);
      res.on('error', () => {});
    });
    req.end();
  });
}

function createSessionHarness({
  limits,
  allowedHosts,
  allowedOrigins,
  keepaliveIntervalMs,
  createMcpServer: customCreateMcpServer,
  TransportBase = SSEServerTransport
} = {}) {
  const servers = [];
  const transports = [];
  class RecordingTransport extends TransportBase {
    constructor(path, response) {
      super(path, response);
      transports.push(this);
    }
  }
  const createServiceNowClient = jest.fn(() => ({}));
  const createMcpServer = jest.fn(customCreateMcpServer ?? (async () => {
    const server = new Server({ name: 'test-server', version: '1.0.0' }, { capabilities: {} });
    jest.spyOn(server, 'close');
    servers.push(server);
    return server;
  }));
  const app = createHttpApp({
    apiToken: TOKEN,
    defaultInstance: DEFAULT_INSTANCE,
    allowedHosts,
    allowedOrigins,
    limits,
    keepaliveIntervalMs,
    createServiceNowClient,
    createMcpServer,
    SSEServerTransport: RecordingTransport
  });
  return { app, servers, transports, createServiceNowClient, createMcpServer };
}

function createManualServer() {
  const server = {
    connect: jest.fn(async (transport) => transport.start()),
    close: jest.fn(async () => {})
  };
  return server;
}

describe('HTTP security configuration', () => {
  test('documents the enforced default limits', () => {
    expect(DEFAULT_HTTP_LIMITS).toMatchObject({
      maxPendingSessions: 16,
      maxActiveSessions: 32,
      maxJsonBytes: 8 * 1024 * 1024,
      maxQueuedPostsPerSession: 8,
      maxConcurrentPosts: 8,
      authFailureLimit: 10,
      authFailureWindowMs: 60_000
    });
  });

  test.each([
    ['missing', undefined],
    ['empty', ''],
    ['short secret', 'release-secret'],
    ['63 hex characters', 'a'.repeat(63)],
    ['65 hex characters', 'a'.repeat(65)],
    ['non-hex 64 characters', 'g'.repeat(64)],
    ['padded base64', `${crypto.randomBytes(32).toString('base64')}`],
    ['non-canonical base64url', `${BASE64URL_TOKEN.slice(0, 42)}B`],
    ['44 base64url characters', `${BASE64URL_TOKEN}A`],
    ['whitespace-wrapped token', ` ${TOKEN.slice(1)}`]
  ])('rejects a %s HAPPY_MCP_API_TOKEN without echoing it', (_label, value) => {
    const failures = [
      () => loadHttpSecurityConfig({ HAPPY_MCP_API_TOKEN: value }),
      () => createHttpApp({ apiToken: value, defaultInstance: DEFAULT_INSTANCE })
    ];
    for (const failure of failures) {
      let error;
      try {
        failure();
      } catch (caught) {
        error = caught;
      }
      expect(error).toBeInstanceOf(Error);
      expect(error.message).toMatch(/HAPPY_MCP_API_TOKEN/);
      if (value) {
        expect(error.message).not.toContain(value.trim());
      }
    }
  });

  test.each([
    ['64 hex characters', TOKEN],
    ['uppercase hex', TOKEN.toUpperCase()],
    ['43 base64url characters', BASE64URL_TOKEN]
  ])('accepts a %s HAPPY_MCP_API_TOKEN', (_label, value) => {
    expect(loadHttpSecurityConfig({ HAPPY_MCP_API_TOKEN: value })).toEqual({
      apiToken: value,
      allowedHosts: [],
      allowedOrigins: []
    });
    expect(() => createHttpApp({ apiToken: value, defaultInstance: DEFAULT_INSTANCE })).not.toThrow();
  });

  test('parses approved reverse-proxy authorities and browser origins', () => {
    expect(loadHttpSecurityConfig({
      HAPPY_MCP_API_TOKEN: TOKEN,
      HAPPY_MCP_ALLOWED_HOSTS: ' mcp.example.com, MCP.Example.com:8443,[::1]:3000,10.0.0.5:3000,mcp_upstream ',
      HAPPY_MCP_ALLOWED_ORIGINS: 'https://mcp.example.com, http://localhost:5173'
    })).toEqual({
      apiToken: TOKEN,
      allowedHosts: ['mcp.example.com', 'mcp.example.com:8443', '[::1]:3000', '10.0.0.5:3000', 'mcp_upstream'],
      allowedOrigins: ['https://mcp.example.com', 'http://localhost:5173']
    });
  });

  test.each([
    '*',
    '*.example.com',
    'http://mcp.example.com',
    'mcp.example.com/path',
    'user@mcp.example.com',
    'mcp.example.com:0',
    'mcp.example.com:65536',
    'mcp.example.com:03000',
    '[::1',
    '[not-ipv6]:3000',
    'bad host.example',
    '-bad.example',
    'mcp..example.com',
    '999.1.1.1'
  ])('rejects malformed or wildcard HAPPY_MCP_ALLOWED_HOSTS entry %p', (entry) => {
    expect(() => loadHttpSecurityConfig({ HAPPY_MCP_API_TOKEN: TOKEN, HAPPY_MCP_ALLOWED_HOSTS: entry }))
      .toThrow(/HAPPY_MCP_ALLOWED_HOSTS/);
    expect(() => createHttpApp({ apiToken: TOKEN, defaultInstance: DEFAULT_INSTANCE, allowedHosts: [entry] }))
      .toThrow(/allowed host/i);
  });

  test.each([
    '*',
    'null',
    'https://*.example.com',
    'https://mcp.example.com/',
    'https://MCP.example.com',
    'mcp.example.com',
    'ftp://mcp.example.com',
    'https://user@mcp.example.com',
    'https://mcp.example.com/path'
  ])('rejects malformed or wildcard HAPPY_MCP_ALLOWED_ORIGINS entry %p', (entry) => {
    expect(() => loadHttpSecurityConfig({ HAPPY_MCP_API_TOKEN: TOKEN, HAPPY_MCP_ALLOWED_ORIGINS: entry }))
      .toThrow(/HAPPY_MCP_ALLOWED_ORIGINS/);
    expect(() => createHttpApp({ apiToken: TOKEN, defaultInstance: DEFAULT_INSTANCE, allowedOrigins: [entry] }))
      .toThrow(/allowed origin/i);
  });

  test.each([
    ['zero', { maxActiveSessions: 0 }],
    ['fractional', { maxJsonBytes: 1.5 }],
    ['unknown', { maxSessions: 1 }]
  ])('rejects %s limit overrides', (_label, limits) => {
    expect(() => createHttpApp({ apiToken: TOKEN, defaultInstance: DEFAULT_INSTANCE, limits })).toThrow(/limit/i);
  });
});

describe('failed-authentication budget', () => {
  test('blocks a peer after the failure limit until its window expires', () => {
    let now = 0;
    const limiter = createAuthFailureLimiter({ limit: 3, windowMs: 1000, maxPeers: 10, now: () => now });

    for (let attempt = 0; attempt < 3; attempt += 1) {
      expect(limiter.retryAfterSeconds('peer-a')).toBeNull();
      expect(limiter.recordFailure('peer-a')).toBe(true);
    }
    expect(limiter.retryAfterSeconds('peer-a')).toBe(1);
    expect(limiter.retryAfterSeconds('peer-b')).toBeNull();

    now = 999;
    expect(limiter.retryAfterSeconds('peer-a')).toBe(1);
    now = 1000;
    expect(limiter.retryAfterSeconds('peer-a')).toBeNull();
    expect(limiter.size).toBe(0);
  });

  test('keeps bounded global state and fails closed for new peers when saturated', () => {
    let now = 0;
    const limiter = createAuthFailureLimiter({ limit: 10, windowMs: 1000, maxPeers: 2, now: () => now });

    expect(limiter.recordFailure('peer-a')).toBe(true);
    now = 100;
    expect(limiter.recordFailure('peer-b')).toBe(true);
    expect(limiter.recordFailure('peer-c')).toBe(false);
    expect(limiter.recordFailure('peer-a')).toBe(true);
    expect(limiter.size).toBe(2);

    now = 1000;
    expect(limiter.recordFailure('peer-c')).toBe(true);
    expect(limiter.size).toBe(2);
  });
});

describe('HTTP request gate', () => {
  function createGateApp(options = {}) {
    const createServiceNowClient = jest.fn(() => ({}));
    const createMcpServer = jest.fn(async () => {
      throw new Error('session factory must not run for a denied request');
    });
    const app = createHttpApp({
      apiToken: TOKEN,
      defaultInstance: DEFAULT_INSTANCE,
      createServiceNowClient,
      createMcpServer,
      ...options
    });
    return { app, createServiceNowClient, createMcpServer };
  }

  const routes = [
    ['GET', '/health'],
    ['GET', '/instances'],
    ['GET', '/mcp'],
    ['POST', '/mcp?sessionId=unknown']
  ];

  test.each(routes)('requires the bearer token for %s %s before any factory runs', async (method, path) => {
    const { app, createServiceNowClient, createMcpServer } = createGateApp();
    const server = await startApp(app);
    try {
      const attempts = [
        {},
        { Authorization: `Bearer ${crypto.randomBytes(32).toString('hex')}` },
        ['Host', `127.0.0.1:${server.port}`, 'Authorization', `Bearer ${TOKEN}`, 'Authorization', `Bearer ${TOKEN}`],
        { Authorization: `Basic ${Buffer.from(`user:${TOKEN}`).toString('base64')}` },
        { Authorization: `Bearer  ${TOKEN}` },
        { Authorization: `Bearer ${TOKEN.slice(0, 32)}` }
      ];
      for (const headers of attempts) {
        const response = await request(server.port, {
          method,
          path,
          headers: Array.isArray(headers)
            ? [...headers, 'Content-Type', 'application/json']
            : { ...headers, 'Content-Type': 'application/json' },
          body: method === 'POST' ? '{"jsonrpc":"2.0","method":"ping","id":1}' : undefined
        });
        expect(response.status).toBe(401);
        expect(response.headers['www-authenticate']).toBe('Bearer');
        expect(JSON.parse(response.body)).toEqual({ error: 'Unauthorized' });
        expect(response.body).not.toContain(TOKEN);
      }
    } finally {
      await server.close();
    }
    expect(createServiceNowClient).not.toHaveBeenCalled();
    expect(createMcpServer).not.toHaveBeenCalled();
  });

  test('serves health and instances only to the configured bearer', async () => {
    const instances = [
      { name: 'dev', url: 'https://dev.example.service-now.com', default: true },
      { name: 'prod', url: 'https://prod.example.service-now.com', default: false }
    ];
    const { app } = createGateApp({ defaultInstance: instances[0], listInstances: () => instances });
    const server = await startApp(app);
    try {
      const health = await request(server.port, { path: '/health' });
      expect(health.status).toBe(200);
      expect(JSON.parse(health.body)).toMatchObject({ status: 'healthy', instance_name: 'dev' });

      const lowercaseScheme = await request(server.port, {
        path: '/instances',
        headers: { Authorization: `bearer ${TOKEN}` }
      });
      expect(lowercaseScheme.status).toBe(200);
      expect(JSON.parse(lowercaseScheme.body)).toEqual({ instances });
    } finally {
      await server.close();
    }
  });

  test('accepts a base64url bearer token', async () => {
    const app = createHttpApp({ apiToken: BASE64URL_TOKEN, defaultInstance: DEFAULT_INSTANCE });
    const server = await startApp(app);
    try {
      const response = await request(server.port, { headers: { Authorization: `Bearer ${BASE64URL_TOKEN}` } });
      expect(response.status).toBe(200);
    } finally {
      await server.close();
    }
  });

  test('authenticates before parsing or routing a POST body', async () => {
    const { app } = createGateApp();
    const server = await startApp(app);
    try {
      const unauthenticated = await request(server.port, {
        method: 'POST',
        path: '/mcp?sessionId=unknown',
        headers: { 'Content-Type': 'application/json' },
        body: '{not json'
      });
      expect(unauthenticated.status).toBe(401);

      const unknownSession = await request(server.port, {
        method: 'POST',
        path: '/mcp?sessionId=unknown',
        headers: { ...AUTH, 'Content-Type': 'application/json' },
        body: '{not json'
      });
      expect(unknownSession.status).toBe(400);
      expect(JSON.parse(unknownSession.body)).toEqual({ error: 'Invalid or missing session ID' });
    } finally {
      await server.close();
    }
  });

  test('throttles repeated failures per peer with 429 before credentials are evaluated', async () => {
    const { app, createMcpServer } = createGateApp({ limits: { authFailureWindowMs: 400 } });
    const server = await startApp(app);
    try {
      for (let attempt = 0; attempt < 10; attempt += 1) {
        const response = await request(server.port, {
          headers: { Authorization: 'Bearer wrong', 'X-Forwarded-For': `198.51.100.${attempt}` }
        });
        expect(response.status).toBe(401);
      }
      const throttled = await request(server.port, { path: '/mcp', headers: { ...AUTH, 'X-Forwarded-For': '203.0.113.9' } });
      expect(throttled.status).toBe(429);
      expect(Number(throttled.headers['retry-after'])).toBeGreaterThanOrEqual(1);
      expect(JSON.parse(throttled.body)).toEqual({ error: 'Too many failed authentication attempts' });
      expect(createMcpServer).not.toHaveBeenCalled();

      await sleep(450);
      const recovered = await request(server.port);
      expect(recovered.status).toBe(200);
    } finally {
      await server.close();
    }
  });

  test.each([
    ['a foreign hostname', () => 'evil.example'],
    ['a foreign hostname at the bound port', (port) => `evil.example:${port}`],
    ['the bound address at another port', (port) => `127.0.0.1:${port + 1}`],
    ['the bound address without its port', () => '127.0.0.1'],
    ['a malformed authority', (port) => `127.0.0.1:${port}/path`],
    ['a wildcard authority', () => '*']
  ])('rejects %s in Host before authentication or setup', async (_label, hostFor) => {
    const { app, createMcpServer } = createGateApp();
    const server = await startApp(app);
    try {
      const response = await request(server.port, {
        path: '/mcp',
        headers: { ...AUTH, Host: hostFor(server.port) }
      });
      expect(response.status).toBe(403);
      expect(JSON.parse(response.body)).toEqual({ error: 'Host not allowed' });
    } finally {
      await server.close();
    }
    expect(createMcpServer).not.toHaveBeenCalled();
  });

  test('rejects duplicate Host headers and ignores forwarded authorities', async () => {
    const { app } = createGateApp({ allowedHosts: ['mcp.example.com'] });
    const server = await startApp(app);
    try {
      const duplicate = await request(server.port, {
        headers: ['Host', `127.0.0.1:${server.port}`, 'Host', `127.0.0.1:${server.port}`, 'Authorization', `Bearer ${TOKEN}`]
      });
      expect(duplicate.status).toBe(403);

      const forwardedOnly = await request(server.port, {
        headers: { ...AUTH, Host: 'evil.example', 'X-Forwarded-Host': 'mcp.example.com', Forwarded: 'host=mcp.example.com' }
      });
      expect(forwardedOnly.status).toBe(403);

      const forwardedIgnored = await request(server.port, {
        headers: { ...AUTH, 'X-Forwarded-Host': 'evil.example', Forwarded: 'host=evil.example' }
      });
      expect(forwardedIgnored.status).toBe(200);
    } finally {
      await server.close();
    }
  });

  test('accepts loopback names on a loopback listener and exact approved authorities', async () => {
    const { app } = createGateApp({ allowedHosts: ['mcp.example.com', 'proxy.example.com:8443'] });
    const server = await startApp(app);
    try {
      for (const host of [`localhost:${server.port}`, `LOCALHOST:${server.port}`, `[::1]:${server.port}`, 'mcp.example.com', 'proxy.example.com:8443']) {
        const response = await request(server.port, { headers: { ...AUTH, Host: host } });
        expect([host, response.status]).toEqual([host, 200]);
      }
      for (const host of ['mcp.example.com:443', 'proxy.example.com', 'sub.mcp.example.com']) {
        const response = await request(server.port, { headers: { ...AUTH, Host: host } });
        expect([host, response.status]).toEqual([host, 403]);
      }
    } finally {
      await server.close();
    }
  });

  (LAN_ADDRESS ? test : test.skip)('accepts only the arriving interface address on a wildcard listener', async () => {
    const { app } = createGateApp({ allowedHosts: ['proxy.example.com:8443'] });
    const listener = app.listen(0, '0.0.0.0');
    await new Promise((resolve) => listener.once('listening', resolve));
    const { port } = listener.address();
    const viaLan = (host) => request(port, { host: LAN_ADDRESS, headers: { ...AUTH, Host: host } });
    const viaLoopback = (host) => request(port, { headers: { ...AUTH, Host: host } });
    try {
      expect((await viaLan(`${LAN_ADDRESS}:${port}`)).status).toBe(200);
      expect((await viaLan('proxy.example.com:8443')).status).toBe(200);
      for (const host of [LAN_ADDRESS, `localhost:${port}`, `127.0.0.1:${port}`, `${LAN_ADDRESS}:${port + 1}`]) {
        const response = await viaLan(host);
        expect([host, response.status]).toEqual([host, 403]);
      }
      expect((await viaLoopback(`localhost:${port}`)).status).toBe(200);
      expect((await viaLoopback(`127.0.0.1:${port}`)).status).toBe(200);
      expect((await viaLoopback(`${LAN_ADDRESS}:${port}`)).status).toBe(403);
    } finally {
      await app.closeAllSessions();
      await new Promise((resolve) => {
        listener.close(resolve);
        listener.closeAllConnections();
      });
    }
  });

  test('allows an absent Origin and exactly approved origins only', async () => {
    const { app, createMcpServer } = createGateApp({ allowedOrigins: ['https://console.example.com'] });
    const server = await startApp(app);
    try {
      expect((await request(server.port)).status).toBe(200);
      expect((await request(server.port, { headers: { ...AUTH, Origin: 'https://console.example.com' } })).status).toBe(200);

      for (const origin of [
        'null',
        '*',
        'https://evil.example',
        'https://console.example.com/',
        'https://console.example.com:443',
        'HTTPS://console.example.com',
        'not a url',
        `http://127.0.0.1:${server.port}`
      ]) {
        const response = await request(server.port, { path: '/mcp', headers: { ...AUTH, Origin: origin } });
        expect([origin, response.status]).toEqual([origin, 403]);
        expect(JSON.parse(response.body)).toEqual({ error: 'Origin not allowed' });
      }
      const duplicate = await request(server.port, {
        headers: [
          'Host', `127.0.0.1:${server.port}`,
          'Authorization', `Bearer ${TOKEN}`,
          'Origin', 'https://console.example.com',
          'Origin', 'https://console.example.com'
        ]
      });
      expect(duplicate.status).toBe(403);
    } finally {
      await server.close();
    }
    expect(createMcpServer).not.toHaveBeenCalled();
  });
});

describe('SSE session lifecycle', () => {
  test('creates an isolated ServiceNow client and MCP server for every session', async () => {
    const configManager = { listInstances: jest.fn() };
    const instanceRegistry = { list: jest.fn() };
    const credentialStore = {};
    const clients = [];
    const createServiceNowClient = jest.fn(() => {
      const client = {};
      clients.push(client);
      return client;
    });
    const createMcpServer = jest.fn(async () => createManualServer());
    const app = createHttpApp({
      apiToken: TOKEN,
      defaultInstance: DEFAULT_INSTANCE,
      configManager,
      instanceRegistry,
      credentialStore,
      createServiceNowClient,
      createMcpServer
    });
    const server = await startApp(app);
    try {
      const first = await openSse(server.port);
      const second = await openSse(server.port);
      expect(first.sessionId).not.toBe(second.sessionId);
      first.close();
      second.close();
    } finally {
      await server.close();
    }

    expect(createServiceNowClient).toHaveBeenCalledTimes(2);
    expect(createServiceNowClient).toHaveBeenCalledWith(DEFAULT_INSTANCE, { credentialStore });
    expect(clients[0]).not.toBe(clients[1]);
    expect(createMcpServer).toHaveBeenNthCalledWith(1, clients[0], { configManager, instanceRegistry, credentialStore });
    expect(createMcpServer).toHaveBeenNthCalledWith(2, clients[1], { configManager, instanceRegistry, credentialStore });
  });

  test('caps pending setups at 16 before awaiting the MCP factory', async () => {
    const setups = [];
    const harness = createSessionHarness({
      createMcpServer: () => {
        const setup = deferred();
        setups.push(setup);
        return setup.promise;
      }
    });
    const server = await startApp(harness.app);
    try {
      const pending = Array.from({ length: 16 }, () => openSse(server.port));
      await waitFor(() => setups.length === 16, '16 pending setups');

      const rejected = await openSse(server.port);
      expect(rejected.status).toBe(429);
      expect(JSON.parse(rejected.body)).toEqual({ error: 'Too many SSE sessions' });
      expect(harness.createServiceNowClient).toHaveBeenCalledTimes(16);
      expect(harness.createMcpServer).toHaveBeenCalledTimes(16);

      for (const setup of setups) setup.resolve(createManualServer());
      const streams = await Promise.all(pending);
      expect(streams.every((stream) => stream.status === 200)).toBe(true);
      for (const stream of streams) stream.close();
    } finally {
      await server.close();
    }
  });

  test('caps active sessions at 32 and recovers capacity after a disconnect', async () => {
    const harness = createSessionHarness();
    const server = await startApp(harness.app);
    try {
      const streams = [];
      for (let index = 0; index < 32; index += 1) {
        const stream = await openSse(server.port);
        expect(stream.status).toBe(200);
        streams.push(stream);
      }
      const rejected = await openSse(server.port);
      expect(rejected.status).toBe(429);
      expect(harness.createMcpServer).toHaveBeenCalledTimes(32);

      streams[0].close();
      await waitFor(() => harness.servers[0].close.mock.calls.length === 1, 'disconnected session cleanup');
      const replacement = await openSse(server.port);
      expect(replacement.status).toBe(200);
      replacement.close();
      for (const stream of streams.slice(1)) stream.close();
    } finally {
      await server.close();
    }
  });

  test('does not install a ghost session when the client disconnects during setup', async () => {
    const setup = deferred();
    let calls = 0;
    const harness = createSessionHarness({
      limits: { maxActiveSessions: 1 },
      createMcpServer: () => {
        calls += 1;
        return calls === 1 ? setup.promise : Promise.resolve(createManualServer());
      }
    });
    const server = await startApp(harness.app);
    try {
      const clientRequest = http.request({ host: '127.0.0.1', port: server.port, path: '/mcp', headers: AUTH, agent: false });
      clientRequest.on('error', () => {});
      clientRequest.end();
      await waitFor(() => calls === 1, 'pending setup');
      const closed = new Promise((resolve) => harness.transports[0].res.once('close', resolve));
      clientRequest.destroy();
      await closed;

      const lateServer = createManualServer();
      setup.resolve(lateServer);
      await waitFor(() => lateServer.close.mock.calls.length === 1, 'late server close');
      expect(lateServer.connect).not.toHaveBeenCalled();

      const ghost = await postMessage(server.port, harness.transports[0].sessionId, { jsonrpc: '2.0', method: 'notifications/initialized' });
      expect(ghost.status).toBe(400);

      const next = await openSse(server.port);
      expect(next.status).toBe(200);
      next.close();
    } finally {
      await server.close();
    }
  });

  test('releases capacity and reports 500 when MCP setup fails', async () => {
    let calls = 0;
    const harness = createSessionHarness({
      limits: { maxActiveSessions: 1 },
      createMcpServer: async () => {
        calls += 1;
        if (calls === 1) throw new Error('setup failed');
        return createManualServer();
      }
    });
    const consoleError = jest.spyOn(console, 'error').mockImplementation(() => {});
    const server = await startApp(harness.app);
    try {
      const failed = await openSse(server.port);
      expect(failed.status).toBe(500);
      expect(JSON.parse(failed.body)).toEqual({ error: 'Failed to establish SSE connection' });

      const next = await openSse(server.port);
      expect(next.status).toBe(200);
      next.close();
    } finally {
      await server.close();
      consoleError.mockRestore();
    }
  });

  test('closes the MCP server, ends the stream and releases capacity when connect fails', async () => {
    const failingServer = {
      connect: jest.fn(async (transport) => {
        await transport.start();
        throw new Error('connect failed');
      }),
      close: jest.fn(async () => {})
    };
    let calls = 0;
    const harness = createSessionHarness({
      limits: { maxActiveSessions: 1 },
      createMcpServer: async () => {
        calls += 1;
        return calls === 1 ? failingServer : createManualServer();
      }
    });
    const consoleError = jest.spyOn(console, 'error').mockImplementation(() => {});
    const server = await startApp(harness.app);
    try {
      const failed = await openSse(server.port);
      await waitFor(() => failed.ended, 'failed stream end');
      expect(failingServer.close).toHaveBeenCalledTimes(1);

      const stale = await postMessage(server.port, failed.sessionId, { jsonrpc: '2.0', method: 'notifications/initialized' });
      expect(stale.status).toBe(400);

      const next = await openSse(server.port);
      expect(next.status).toBe(200);
      next.close();
    } finally {
      await server.close();
      consoleError.mockRestore();
    }
  });

  test('cleans up exactly once when close paths repeat', async () => {
    const harness = createSessionHarness({ limits: { maxActiveSessions: 1 } });
    const server = await startApp(harness.app);
    try {
      const stream = await openSse(server.port);
      const [transport] = harness.transports;
      transport.onclose();
      transport.onclose();
      await transport.close();
      stream.close();
      await waitFor(() => stream.ended, 'stream end');
      await sleep(20);
      expect(harness.servers[0].close).toHaveBeenCalledTimes(1);

      const next = await openSse(server.port);
      expect(next.status).toBe(200);
      next.close();
    } finally {
      await server.close();
    }
  });

  test('stops keepalive writes once the stream closes', async () => {
    const harness = createSessionHarness({ keepaliveIntervalMs: 10 });
    const server = await startApp(harness.app);
    try {
      const stream = await openSse(server.port);
      await waitFor(() => stream.keepalives >= 2, 'keepalive comments');
      const response = harness.transports[0].res;
      const write = jest.spyOn(response, 'write');
      stream.close();
      await waitFor(() => harness.servers[0].close.mock.calls.length === 1, 'session cleanup');
      const writesAtClose = write.mock.calls.length;
      await sleep(60);
      expect(write.mock.calls.length).toBe(writesAtClose);
    } finally {
      await server.close();
    }
  });

  test('answers HEAD /mcp with 405 without admitting a session', async () => {
    const harness = createSessionHarness({ limits: { maxActiveSessions: 1 } });
    const server = await startApp(harness.app);
    try {
      const unauthenticated = await request(server.port, { method: 'HEAD', path: '/mcp', headers: {} });
      expect(unauthenticated.status).toBe(401);

      const head = await request(server.port, { method: 'HEAD', path: '/mcp' });
      expect(head.status).toBe(405);
      expect(head.headers.allow).toBe('GET, POST');
      expect(harness.createServiceNowClient).not.toHaveBeenCalled();
      expect(harness.createMcpServer).not.toHaveBeenCalled();

      const stream = await openSse(server.port);
      expect(stream.status).toBe(200);
      stream.close();
    } finally {
      await server.close();
    }
  });

  test('tolerates repeated stream errors and cleans up once', async () => {
    const harness = createSessionHarness();
    const server = await startApp(harness.app);
    try {
      const stream = await openSse(server.port);
      const response = harness.transports[0].res;
      expect(() => response.emit('error', new Error('first response error'))).not.toThrow();
      expect(() => response.emit('error', new Error('second response error'))).not.toThrow();
      expect(() => response.req.emit('error', new Error('first request error'))).not.toThrow();
      expect(() => response.req.emit('error', new Error('second request error'))).not.toThrow();
      await waitFor(() => stream.ended, 'stream end');
      expect(harness.servers[0].close).toHaveBeenCalledTimes(1);
    } finally {
      await server.close();
    }
  });

  test('expires idle sessions and frees their capacity', async () => {
    const harness = createSessionHarness({ limits: { maxActiveSessions: 1, sessionIdleTimeoutMs: 100 } });
    const server = await startApp(harness.app);
    try {
      const stream = await openSse(server.port);
      await waitFor(() => stream.ended, 'idle expiry', 2000);
      expect(harness.servers[0].close).toHaveBeenCalledTimes(1);
      const next = await openSse(server.port);
      expect(next.status).toBe(200);
      next.close();
    } finally {
      await server.close();
    }
  });

  test('treats POST traffic as activity that postpones idle expiry', async () => {
    const harness = createSessionHarness({ limits: { sessionIdleTimeoutMs: 300 } });
    const server = await startApp(harness.app);
    try {
      const stream = await openSse(server.port);
      for (let tick = 0; tick < 10; tick += 1) {
        await sleep(60);
        const response = await postMessage(server.port, stream.sessionId, { jsonrpc: '2.0', method: 'notifications/initialized' });
        expect(response.status).toBe(202);
      }
      expect(stream.ended).toBe(false);
      await waitFor(() => stream.ended, 'idle expiry after activity stops', 2000);
    } finally {
      await server.close();
    }
  });

  test('enforces a maximum session lifetime despite activity', async () => {
    const harness = createSessionHarness({ limits: { sessionIdleTimeoutMs: 10_000, sessionMaxLifetimeMs: 200 } });
    const server = await startApp(harness.app);
    try {
      const stream = await openSse(server.port);
      const startedAt = Date.now();
      while (!stream.ended && Date.now() - startedAt < 2000) {
        await postMessage(server.port, stream.sessionId, { jsonrpc: '2.0', method: 'notifications/initialized' });
        await sleep(30);
      }
      expect(stream.ended).toBe(true);
      expect(harness.servers[0].close).toHaveBeenCalledTimes(1);
    } finally {
      await server.close();
    }
  });

  test('times out a stalled setup and releases its pending slot', async () => {
    const stalled = deferred();
    let calls = 0;
    const harness = createSessionHarness({
      limits: { maxActiveSessions: 1, sessionSetupTimeoutMs: 100 },
      createMcpServer: () => {
        calls += 1;
        return calls === 1 ? stalled.promise : Promise.resolve(createManualServer());
      }
    });
    const server = await startApp(harness.app);
    try {
      const timedOut = await openSse(server.port);
      expect(timedOut.status).toBe(503);
      expect(JSON.parse(timedOut.body)).toEqual({ error: 'SSE session setup timed out' });

      const next = await openSse(server.port);
      expect(next.status).toBe(200);

      const lateServer = createManualServer();
      stalled.resolve(lateServer);
      await waitFor(() => lateServer.close.mock.calls.length === 1, 'late server close');
      expect(lateServer.connect).not.toHaveBeenCalled();
      next.close();
    } finally {
      await server.close();
    }
  });

  test('closeAllSessions closes active and pending sessions and refuses new ones', async () => {
    const pendingSetup = deferred();
    let calls = 0;
    const harness = createSessionHarness({
      createMcpServer: async () => {
        calls += 1;
        if (calls === 1) {
          const activeServer = new Server({ name: 'test-server', version: '1.0.0' }, { capabilities: {} });
          jest.spyOn(activeServer, 'close');
          harness.servers.push(activeServer);
          return activeServer;
        }
        return pendingSetup.promise;
      }
    });
    const server = await startApp(harness.app);
    try {
      const active = await openSse(server.port);
      const pending = openSse(server.port);
      await waitFor(() => calls === 2, 'pending setup');

      await harness.app.closeAllSessions();
      await waitFor(() => active.ended, 'active stream end');
      expect(harness.servers[0].close).toHaveBeenCalledTimes(1);
      expect((await pending).status).toBe(503);

      const lateServer = createManualServer();
      pendingSetup.resolve(lateServer);
      await waitFor(() => lateServer.close.mock.calls.length === 1, 'late server close');

      const refused = await openSse(server.port);
      expect(refused.status).toBe(503);
      expect(JSON.parse(refused.body)).toEqual({ error: 'Server is shutting down' });
    } finally {
      await server.close();
    }
  });
});

describe('SSE POST limits', () => {
  test('refuses missing, unknown and closed session ids before the SDK transport', async () => {
    const harness = createSessionHarness();
    const handlePostMessage = jest.spyOn(SSEServerTransport.prototype, 'handlePostMessage');
    const server = await startApp(harness.app);
    try {
      const message = { jsonrpc: '2.0', method: 'notifications/initialized' };
      for (const path of ['/mcp', '/mcp?sessionId=unknown', '/mcp?sessionId=a&sessionId=b']) {
        const response = await request(server.port, {
          method: 'POST',
          path,
          headers: { ...AUTH, 'Content-Type': 'application/json' },
          body: JSON.stringify(message)
        });
        expect([path, response.status]).toEqual([path, 400]);
      }

      const stream = await openSse(server.port);
      stream.close();
      await waitFor(() => harness.servers[0].close.mock.calls.length === 1, 'session cleanup');
      const closed = await postMessage(server.port, stream.sessionId, message);
      expect(closed.status).toBe(400);
      expect(handlePostMessage).not.toHaveBeenCalled();
    } finally {
      handlePostMessage.mockRestore();
      await server.close();
    }
  });

  test('rejects non-JSON bodies and JSON bodies above 8 MiB without dispatching them', async () => {
    const harness = createSessionHarness();
    const handlePostMessage = jest.spyOn(SSEServerTransport.prototype, 'handlePostMessage');
    const server = await startApp(harness.app);
    try {
      const stream = await openSse(server.port);
      const textBody = await request(server.port, {
        method: 'POST',
        path: `/mcp?sessionId=${stream.sessionId}`,
        headers: { ...AUTH, 'Content-Type': 'text/plain' },
        body: 'hello'
      });
      expect(textBody.status).toBe(415);

      const limit = DEFAULT_HTTP_LIMITS.maxJsonBytes;
      const envelope = { jsonrpc: '2.0', method: 'notifications/padding', params: { pad: '' } };
      const padding = limit - Buffer.byteLength(JSON.stringify(envelope));
      envelope.params.pad = 'x'.repeat(padding + 1);
      const oversized = await postMessage(server.port, stream.sessionId, JSON.stringify(envelope));
      expect(oversized.status).toBe(413);
      expect(JSON.parse(oversized.body)).toEqual({ error: 'Request body too large' });
      expect(handlePostMessage).not.toHaveBeenCalled();

      envelope.params.pad = 'x'.repeat(padding);
      const atLimit = JSON.stringify(envelope);
      expect(Buffer.byteLength(atLimit)).toBe(limit);
      const accepted = await postMessage(server.port, stream.sessionId, atLimit);
      expect(accepted.status).toBe(202);
      expect(handlePostMessage).toHaveBeenCalledTimes(1);

      const malformed = await postMessage(server.port, stream.sessionId, '{not json');
      expect(malformed.status).toBe(400);
      expect(JSON.parse(malformed.body)).toEqual({ error: 'Invalid JSON body' });
      stream.close();
    } finally {
      handlePostMessage.mockRestore();
      await server.close();
    }
  });

  test('processes one POST at a time per session and bounds the per-session queue', async () => {
    const gate = deferred();
    let active = 0;
    let maxActive = 0;
    class GatedTransport extends SSEServerTransport {
      async handlePostMessage(req, res, body) {
        active += 1;
        maxActive = Math.max(maxActive, active);
        try {
          await gate.promise;
          return await super.handlePostMessage(req, res, body);
        } finally {
          active -= 1;
        }
      }
    }
    const harness = createSessionHarness({ TransportBase: GatedTransport });
    const server = await startApp(harness.app);
    try {
      const stream = await openSse(server.port);
      const message = { jsonrpc: '2.0', method: 'notifications/initialized' };
      const admitted = Array.from({ length: 9 }, () => postMessage(server.port, stream.sessionId, message));
      await waitFor(() => active === 1, 'first POST in flight');
      await sleep(100);

      const overflow = await postMessage(server.port, stream.sessionId, message);
      expect(overflow.status).toBe(429);
      expect(JSON.parse(overflow.body)).toEqual({ error: 'Too many queued requests for this session' });
      expect(active).toBe(1);

      gate.resolve();
      const responses = await Promise.all(admitted);
      expect(responses.map((response) => response.status)).toEqual(Array(9).fill(202));
      expect(maxActive).toBe(1);
      stream.close();
    } finally {
      await server.close();
    }
  });

  test('bounds concurrent POST processing across sessions', async () => {
    const gate = deferred();
    let active = 0;
    let maxActive = 0;
    class GatedTransport extends SSEServerTransport {
      async handlePostMessage(req, res, body) {
        active += 1;
        maxActive = Math.max(maxActive, active);
        try {
          await gate.promise;
          return await super.handlePostMessage(req, res, body);
        } finally {
          active -= 1;
        }
      }
    }
    const harness = createSessionHarness({ TransportBase: GatedTransport, limits: { maxConcurrentPosts: 2 } });
    const server = await startApp(harness.app);
    try {
      const streams = await Promise.all([openSse(server.port), openSse(server.port), openSse(server.port)]);
      const message = { jsonrpc: '2.0', method: 'notifications/initialized' };
      const posts = streams.map((stream) => postMessage(server.port, stream.sessionId, message));
      await waitFor(() => active === 2, 'two POSTs in flight');
      await sleep(30);
      expect(active).toBe(2);

      gate.resolve();
      expect((await Promise.all(posts)).map((response) => response.status)).toEqual([202, 202, 202]);
      expect(maxActive).toBe(2);
      for (const stream of streams) stream.close();
    } finally {
      await server.close();
    }
  });

  test('bounds unanswered JSON-RPC requests per session and process-wide', async () => {
    const gates = [];
    const harness = createSessionHarness({
      limits: { maxOutstandingRequestsPerSession: 2, maxOutstandingRequests: 3 },
      createMcpServer: async () => {
        const mcpServer = new Server({ name: 'test-server', version: '1.0.0' }, { capabilities: { tools: {} } });
        mcpServer.setRequestHandler(CallToolRequestSchema, async () => {
          const gate = deferred();
          gates.push(gate);
          await gate.promise;
          return { content: [{ type: 'text', text: 'done' }] };
        });
        jest.spyOn(mcpServer, 'close');
        harness.servers.push(mcpServer);
        return mcpServer;
      }
    });
    const server = await startApp(harness.app);
    const call = (id) => ({ jsonrpc: '2.0', id, method: 'tools/call', params: { name: 'slow', arguments: {} } });
    try {
      const first = await openSse(server.port);
      const second = await openSse(server.port);

      expect((await postMessage(server.port, first.sessionId, call(1))).status).toBe(202);
      expect((await postMessage(server.port, first.sessionId, call(1))).status).toBe(400);
      expect((await postMessage(server.port, first.sessionId, call(2))).status).toBe(202);
      const perSession = await postMessage(server.port, first.sessionId, call(3));
      expect(perSession.status).toBe(429);
      expect(JSON.parse(perSession.body)).toEqual({ error: 'Too many outstanding requests' });

      expect((await postMessage(server.port, second.sessionId, call('a'))).status).toBe(202);
      expect((await postMessage(server.port, second.sessionId, call('b'))).status).toBe(429);
      await waitFor(() => gates.length === 3, 'three running tool calls');

      gates[0].resolve();
      await waitFor(() => first.events.some((event) => event.event === 'message' && JSON.parse(event.data).id === 1), 'first response');
      expect((await postMessage(server.port, first.sessionId, call(3))).status).toBe(202);

      const cancelled = await postMessage(server.port, second.sessionId, {
        jsonrpc: '2.0',
        method: 'notifications/cancelled',
        params: { requestId: 'a', reason: 'test' }
      });
      expect(cancelled.status).toBe(202);
      // The cancelled handler is still running, so its slot stays taken.
      const stillRunning = await postMessage(server.port, second.sessionId, call('b'));
      expect(stillRunning.status).toBe(429);
      const unknownCancel = await postMessage(server.port, second.sessionId, {
        jsonrpc: '2.0',
        method: 'notifications/cancelled',
        params: { requestId: 'never-sent', reason: 'test' }
      });
      expect(unknownCancel.status).toBe(202);

      gates[2].resolve();
      await sleep(50);
      expect((await postMessage(server.port, second.sessionId, call('b'))).status).toBe(202);
      expect(second.events.some((event) => event.event === 'message' && JSON.parse(event.data).id === 'a')).toBe(false);

      for (const gate of gates) gate.resolve();
      await waitFor(() => second.events.some((event) => event.event === 'message' && JSON.parse(event.data).id === 'b'), 'response to b');
      first.close();
      second.close();
    } finally {
      await server.close();
    }
  });
});

test('creates an authorization_code client with every OAuth option', () => {
  const client = createDefaultClient({
    name: 'public-oauth',
    url: 'https://example.service-now.com',
    authType: 'oauth',
    grantType: 'authorization_code',
    clientId: 'public-client',
    authorizeUrl: 'https://example.service-now.com/oauth_auth.do',
    tokenUrl: 'https://example.service-now.com/oauth_token.do',
    redirectPort: 8455,
    callbackPath: '/callback'
  });

  expect(client.oauthConfig).toMatchObject({
    grantType: 'authorization_code',
    clientId: 'public-client',
    authorizeUrl: 'https://example.service-now.com/oauth_auth.do',
    tokenUrl: 'https://example.service-now.com/oauth_token.do',
    redirectPort: 8455,
    callbackPath: '/callback'
  });
});

test('passes one injected credential store through default clients', () => {
  const credentialStore = {};
  const client = createDefaultClient({
    name: 'credentialed',
    url: 'https://example.service-now.com',
    authType: 'basic',
    username: 'user',
    credentialRef: 'keychain:instance/credentialed/password'
  }, { credentialStore });
  expect(client._credentialStore).toBe(credentialStore);
});
