import { createHash, timingSafeEqual } from 'node:crypto';
import { isIP } from 'node:net';
import express from 'express';
import { SSEServerTransport } from '@modelcontextprotocol/sdk/server/sse.js';
import { ServiceNowClient } from './servicenow-client.js';
import { createMcpServer } from './mcp-server-consolidated.js';
import { instanceToClientOptions } from './config-manager.js';
import { InstanceCredentialStore } from './instance-credential-store.js';
import { installProcessCrashGuards } from './process-guards.js';

// Guards the whole process against a single unhandled rejection or
// uncaught exception taking down every concurrent MCP session — see
// process-guards.js for the full rationale. Registration happens once
// per process (module-scoped flag), so whichever entrypoint imports
// this module first wins the label; server.js imports http-server.js,
// so 'http-server' is the label actually used there too.
installProcessCrashGuards('http-server');

/**
 * Resource bounds for the single-principal HTTP/SSE transport. Every value is
 * enforced in-process; nothing is shared between processes or replicas.
 */
export const DEFAULT_HTTP_LIMITS = Object.freeze({
  maxPendingSessions: 16,
  maxActiveSessions: 32,
  sessionSetupTimeoutMs: 30_000,
  sessionIdleTimeoutMs: 30 * 60_000,
  sessionMaxLifetimeMs: 12 * 60 * 60_000,
  maxJsonBytes: 8 * 1024 * 1024,
  maxQueuedPostsPerSession: 8,
  maxConcurrentPosts: 8,
  maxOutstandingRequestsPerSession: 16,
  maxOutstandingRequests: 64,
  authFailureLimit: 10,
  authFailureWindowMs: 60_000,
  maxTrackedAuthPeers: 1024
});

const TOKEN_GUIDANCE = 'generate one with `openssl rand -hex 32`';
const HEX_TOKEN = /^[0-9A-Fa-f]{64}$/;
const BASE64URL_TOKEN = /^[A-Za-z0-9_-]{43}$/;
const BEARER_CREDENTIALS = /^Bearer ([A-Za-z0-9._~+/-]+=*)$/i;
const AUTHORITY = /^(?:\[([0-9A-Fa-f:.]+)\]|([A-Za-z0-9._-]+))(?::([1-9][0-9]{0,4}))?$/;
// LDH labels plus "_", which container and compose service names may contain.
const DNS_LABEL = /^[a-z0-9_](?:[a-z0-9_-]{0,61}[a-z0-9_])?$/;
const MAX_AUTHORITY_LENGTH = 262;
const LOOPBACK_NAMES = new Set(['localhost', '127.0.0.1', '[::1]']);

function validateApiToken(apiToken) {
  if (typeof apiToken !== 'string' || apiToken.length === 0) {
    throw new Error(`HAPPY_MCP_API_TOKEN is required for the HTTP transport; ${TOKEN_GUIDANCE}`);
  }
  const canonicalBase64url = BASE64URL_TOKEN.test(apiToken)
    && Buffer.from(apiToken, 'base64url').toString('base64url') === apiToken;
  if (!HEX_TOKEN.test(apiToken) && !canonicalBase64url) {
    throw new Error(
      `HAPPY_MCP_API_TOKEN must be 32 random bytes encoded as 64 hex or 43 base64url characters; ${TOKEN_GUIDANCE}`
    );
  }
  return apiToken;
}

function formatAddress(address) {
  if (isIP(address) !== 6) {
    return address;
  }
  try {
    return new URL(`http://[${address}]/`).hostname;
  } catch {
    return `[${address.toLowerCase()}]`;
  }
}

function parseAuthority(value) {
  if (typeof value !== 'string' || value.length === 0 || value.length > MAX_AUTHORITY_LENGTH) {
    return null;
  }
  const match = AUTHORITY.exec(value);
  if (!match) {
    return null;
  }
  const [, ipv6, name, portText] = match;
  const port = portText === undefined ? null : Number(portText);
  if (port !== null && port > 65535) {
    return null;
  }
  if (ipv6 !== undefined) {
    return isIP(ipv6) === 6 ? { hostname: formatAddress(ipv6), port } : null;
  }
  const hostname = name.toLowerCase();
  if (/^[0-9.]+$/.test(hostname)) {
    return isIP(hostname) === 4 ? { hostname, port } : null;
  }
  if (hostname.length > 253 || !hostname.split('.').every((label) => DNS_LABEL.test(label))) {
    return null;
  }
  return { hostname, port };
}

function formatAuthority({ hostname, port }) {
  return port === null ? hostname : `${hostname}:${port}`;
}

function parseAllowedHosts(entries) {
  if (!Array.isArray(entries)) {
    throw new Error('allowedHosts must be an array of allowed host entries');
  }
  return [...new Set(entries.map((entry) => {
    const authority = parseAuthority(entry);
    if (!authority) {
      throw new Error(
        `Invalid allowed host entry ${JSON.stringify(entry)}: expected host or host:port without scheme, path or wildcard`
      );
    }
    return formatAuthority(authority);
  }))];
}

function parseAllowedOrigins(entries) {
  if (!Array.isArray(entries)) {
    throw new Error('allowedOrigins must be an array of allowed origin entries');
  }
  return [...new Set(entries.map((entry) => {
    let url = null;
    try {
      url = new URL(entry);
    } catch {
      url = null;
    }
    if (
      typeof entry !== 'string'
      || entry.includes('*')
      || !url
      || (url.protocol !== 'http:' && url.protocol !== 'https:')
      || url.origin !== entry
    ) {
      throw new Error(
        `Invalid allowed origin entry ${JSON.stringify(entry)}: expected an exact scheme://host[:port] origin without path or wildcard`
      );
    }
    return entry;
  }))];
}

function splitList(value) {
  if (typeof value !== 'string') {
    return [];
  }
  return value.split(',').map((entry) => entry.trim()).filter(Boolean);
}

function withEnvName(name, parse) {
  try {
    return parse();
  } catch (error) {
    throw new Error(`${name}: ${error.message}`);
  }
}

/**
 * Reads and validates the HTTP transport security settings from the
 * environment. Throws (without echoing the token) when they are unusable.
 */
export function loadHttpSecurityConfig(env = process.env) {
  return {
    apiToken: validateApiToken(env.HAPPY_MCP_API_TOKEN),
    allowedHosts: withEnvName('HAPPY_MCP_ALLOWED_HOSTS', () => parseAllowedHosts(splitList(env.HAPPY_MCP_ALLOWED_HOSTS))),
    allowedOrigins: withEnvName(
      'HAPPY_MCP_ALLOWED_ORIGINS',
      () => parseAllowedOrigins(splitList(env.HAPPY_MCP_ALLOWED_ORIGINS))
    )
  };
}

function resolveLimits(overrides) {
  const limits = { ...DEFAULT_HTTP_LIMITS };
  for (const [key, value] of Object.entries(overrides ?? {})) {
    if (!Object.hasOwn(DEFAULT_HTTP_LIMITS, key)) {
      throw new Error(`Unknown HTTP limit "${key}"`);
    }
    if (!Number.isSafeInteger(value) || value <= 0) {
      throw new Error(`HTTP limit "${key}" must be a positive integer`);
    }
    limits[key] = value;
  }
  return limits;
}

/**
 * Fixed-window failed-authentication budget keyed by socket peer address.
 * State is bounded by maxPeers; once every tracked window is live, failures
 * from untracked peers are refused (fail closed) until a window expires.
 */
export function createAuthFailureLimiter({ limit, windowMs, maxPeers, now = () => performance.now() }) {
  const peers = new Map();
  const isExpired = (entry, time) => time - entry.windowStart >= windowMs;
  const pruneExpired = (time) => {
    // Map iteration follows insertion order, which is window-start order.
    for (const [peer, entry] of peers) {
      if (!isExpired(entry, time)) {
        break;
      }
      peers.delete(peer);
    }
  };

  return {
    get size() {
      return peers.size;
    },
    retryAfterSeconds(peer) {
      const time = now();
      const entry = peers.get(peer);
      if (!entry) {
        return null;
      }
      if (isExpired(entry, time)) {
        peers.delete(peer);
        return null;
      }
      if (entry.failures < limit) {
        return null;
      }
      return Math.max(1, Math.ceil((entry.windowStart + windowMs - time) / 1000));
    },
    recordFailure(peer) {
      const time = now();
      let entry = peers.get(peer);
      if (entry && isExpired(entry, time)) {
        peers.delete(peer);
        entry = undefined;
      }
      if (!entry) {
        if (peers.size >= maxPeers) {
          pruneExpired(time);
        }
        if (peers.size >= maxPeers) {
          return false;
        }
        entry = { windowStart: time, failures: 0 };
        peers.set(peer, entry);
      }
      entry.failures += 1;
      return true;
    }
  };
}

function createSlotQueue(maxActive, maxWaiting) {
  let active = 0;
  const waiting = [];
  return {
    acquire() {
      if (active < maxActive) {
        active += 1;
        return Promise.resolve(true);
      }
      if (waiting.length >= maxWaiting) {
        return null;
      }
      return new Promise((resolve) => waiting.push(resolve));
    },
    release() {
      const next = waiting.shift();
      if (next) {
        next(true);
      } else {
        active -= 1;
      }
    },
    close() {
      for (const resolve of waiting.splice(0)) {
        resolve(false);
      }
    }
  };
}

function normalizeAddress(address) {
  if (typeof address !== 'string' || address.length === 0) {
    return null;
  }
  if (address.startsWith('::ffff:') && isIP(address.slice(7)) === 4) {
    return address.slice(7);
  }
  return address;
}

function isLoopbackAddress(address) {
  return address === '::1' || (isIP(address) === 4 && address.startsWith('127.'));
}

function hostAllowed(req, allowedHosts) {
  const values = req.headersDistinct.host;
  if (!values || values.length !== 1) {
    return false;
  }
  const authority = parseAuthority(values[0]);
  if (!authority) {
    return false;
  }
  if (allowedHosts.has(formatAuthority(authority))) {
    return true;
  }
  if ((authority.port ?? 80) !== req.socket.localPort) {
    return false;
  }
  const localAddress = normalizeAddress(req.socket.localAddress);
  if (!localAddress) {
    return false;
  }
  if (authority.hostname === formatAddress(localAddress)) {
    return true;
  }
  return isLoopbackAddress(localAddress) && LOOPBACK_NAMES.has(authority.hostname);
}

function originAllowed(req, allowedOrigins) {
  const values = req.headersDistinct.origin;
  if (values === undefined) {
    return true;
  }
  return values.length === 1 && allowedOrigins.has(values[0]);
}

function bearerMatches(req, expectedDigest) {
  const values = req.headersDistinct.authorization;
  if (!values || values.length !== 1) {
    return false;
  }
  const match = BEARER_CREDENTIALS.exec(values[0]);
  if (!match) {
    return false;
  }
  return timingSafeEqual(createHash('sha256').update(match[1]).digest(), expectedDigest);
}

function isWritable(res) {
  return !res.writableEnded && !res.destroyed && !res.socket?.destroyed;
}

function sendJson(res, status, body, headers = {}) {
  if (res.headersSent || !isWritable(res)) {
    return;
  }
  res.set(headers);
  res.status(status).json(body);
}

function isJsonRpcId(id) {
  return typeof id === 'string' || (typeof id === 'number' && Number.isFinite(id));
}

function requestIdKey(id) {
  return `${typeof id}:${id}`;
}

function isPlainObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function jsonRpcRequestKey(message) {
  if (isPlainObject(message) && typeof message.method === 'string' && isJsonRpcId(message.id)) {
    return requestIdKey(message.id);
  }
  return null;
}

function jsonRpcResponseKey(message) {
  if (
    isPlainObject(message)
    && message.method === undefined
    && isJsonRpcId(message.id)
    && ('result' in message || 'error' in message)
  ) {
    return requestIdKey(message.id);
  }
  return null;
}

function cancelledRequestKey(message) {
  if (
    isPlainObject(message)
    && message.method === 'notifications/cancelled'
    && message.id === undefined
    && isJsonRpcId(message.params?.requestId)
  ) {
    return requestIdKey(message.params.requestId);
  }
  return null;
}

function runMiddleware(middleware, req, res) {
  return new Promise((resolve, reject) => {
    middleware(req, res, (error) => (error ? reject(error) : resolve()));
  });
}

function closeMcpServer(server) {
  return Promise.resolve()
    .then(() => server?.close?.())
    .catch((error) => console.error('Error closing MCP server:', error?.message));
}

export function createDefaultClient(instance, { credentialStore } = {}) {
  const client = new ServiceNowClient(
    instance.url,
    instance.username,
    instance.password,
    instanceToClientOptions(instance, { credentialStore })
  );
  client.currentInstanceName = instance.name;
  return client;
}

/**
 * Builds the authenticated HTTP/SSE app for one trusted operator. Every route
 * requires `Authorization: Bearer <HAPPY_MCP_API_TOKEN>`, a Host that names
 * this listener (or an approved proxy authority) and, when present, an
 * approved Origin. The returned app exposes `closeAllSessions()` for shutdown.
 */
export function createHttpApp({
  defaultInstance,
  apiToken,
  allowedHosts = [],
  allowedOrigins = [],
  limits: limitOverrides = {},
  configManager,
  instanceRegistry,
  credentialStore = new InstanceCredentialStore(),
  keepaliveIntervalMs = 15000,
  listInstances = () => [{
    name: defaultInstance.name,
    url: defaultInstance.url,
    default: true,
    description: defaultInstance.description || ''
  }],
  createServiceNowClient = createDefaultClient,
  createMcpServer: createServer = createMcpServer,
  SSEServerTransport: Transport = SSEServerTransport
} = {}) {
  const expectedDigest = createHash('sha256').update(validateApiToken(apiToken)).digest();
  const approvedHosts = new Set(parseAllowedHosts(allowedHosts));
  const approvedOrigins = new Set(parseAllowedOrigins(allowedOrigins));
  const limits = resolveLimits(limitOverrides);
  if (!Number.isSafeInteger(keepaliveIntervalMs) || keepaliveIntervalMs <= 0) {
    throw new Error('keepaliveIntervalMs must be a positive integer');
  }

  const authFailures = createAuthFailureLimiter({
    limit: limits.authFailureLimit,
    windowMs: limits.authFailureWindowMs,
    maxPeers: limits.maxTrackedAuthPeers
  });
  const parseJson = express.json({ limit: limits.maxJsonBytes });
  const postSlots = createSlotQueue(limits.maxConcurrentPosts, limits.maxActiveSessions);
  const liveSessions = new Set();
  const sessions = new Map();
  let outstandingRequests = 0;
  let shuttingDown = false;

  const app = express();
  app.disable('x-powered-by');

  function rejectRepeatedFailures(res, retryAfterSeconds) {
    sendJson(res, 429, { error: 'Too many failed authentication attempts' }, {
      'Retry-After': String(retryAfterSeconds)
    });
  }

  // Runs before every route and before any body parsing or session allocation.
  app.use((req, res, next) => {
    const peer = normalizeAddress(req.socket.remoteAddress) ?? 'unknown';
    const retryAfter = authFailures.retryAfterSeconds(peer);
    if (retryAfter !== null) {
      return rejectRepeatedFailures(res, retryAfter);
    }
    if (!hostAllowed(req, approvedHosts)) {
      return sendJson(res, 403, { error: 'Host not allowed' });
    }
    if (!originAllowed(req, approvedOrigins)) {
      return sendJson(res, 403, { error: 'Origin not allowed' });
    }
    if (!bearerMatches(req, expectedDigest)) {
      if (!authFailures.recordFailure(peer)) {
        return rejectRepeatedFailures(res, Math.ceil(limits.authFailureWindowMs / 1000));
      }
      return sendJson(res, 401, { error: 'Unauthorized' }, { 'WWW-Authenticate': 'Bearer' });
    }
    return next();
  });

  function markActivity(session) {
    session.idleTimer?.refresh();
  }

  function releaseOutstanding(session, key) {
    if (session.outstanding.delete(key)) {
      outstandingRequests -= 1;
    }
  }

  function closeSession(session, failure) {
    if (session.closed) {
      return session.closing;
    }
    session.closed = true;
    liveSessions.delete(session);
    if (session.id !== undefined && sessions.get(session.id) === session) {
      sessions.delete(session.id);
    }
    clearTimeout(session.setupTimer);
    clearTimeout(session.idleTimer);
    clearTimeout(session.lifetimeTimer);
    clearInterval(session.keepaliveTimer);
    session.posts.close();
    outstandingRequests -= session.outstanding.size;
    session.outstanding.clear();
    session.cancelled.clear();

    const res = session.response;
    if (failure && !res.headersSent) {
      sendJson(res, failure.status, { error: failure.error });
    } else if (isWritable(res)) {
      res.end();
    }

    const { server } = session;
    session.closing = server ? closeMcpServer(server) : Promise.resolve();
    return session.closing;
  }

  function instrumentTransport(session) {
    const { transport } = session;
    const send = transport.send.bind(transport);
    transport.send = (message, ...rest) => {
      if (!session.closed) {
        markActivity(session);
        const key = jsonRpcResponseKey(message);
        if (key !== null) {
          releaseOutstanding(session, key);
          if (session.cancelled.delete(key)) {
            return Promise.resolve();
          }
        }
      }
      return send(message, ...rest);
    };
  }

  function startSessionTimers(session) {
    const res = session.response;
    session.idleTimer = setTimeout(() => closeSession(session), limits.sessionIdleTimeoutMs);
    session.lifetimeTimer = setTimeout(() => closeSession(session), limits.sessionMaxLifetimeMs);
    session.keepaliveTimer = setInterval(() => {
      if (isWritable(res)) {
        res.write(': keepalive\n\n');
      }
    }, keepaliveIntervalMs);
  }

  async function openSession(req, res) {
    const session = {
      closed: false,
      closing: undefined,
      response: res,
      server: null,
      transport: null,
      id: undefined,
      posts: createSlotQueue(1, limits.maxQueuedPostsPerSession),
      outstanding: new Set(),
      cancelled: new Set()
    };
    // Admission and cleanup are registered before the first await so a
    // disconnect during setup always releases the slot.
    liveSessions.add(session);
    const onDisconnect = () => closeSession(session);
    res.once('close', onDisconnect);
    res.on('error', onDisconnect);
    req.on('error', onDisconnect);
    session.setupTimer = setTimeout(
      () => closeSession(session, { status: 503, error: 'SSE session setup timed out' }),
      limits.sessionSetupTimeoutMs
    );

    try {
      res.setHeader('Cache-Control', 'no-cache, no-transform');
      res.setHeader('X-Accel-Buffering', 'no');
      res.setHeader('Connection', 'keep-alive');
      const transport = new Transport('/mcp', res);
      session.transport = transport;
      session.id = transport.sessionId;
      transport.onclose = onDisconnect;
      instrumentTransport(session);

      const serviceNowClient = createServiceNowClient(defaultInstance, { credentialStore });
      const server = await createServer(serviceNowClient, {
        configManager,
        instanceRegistry,
        credentialStore
      });
      if (session.closed) {
        await closeMcpServer(server);
        return;
      }
      session.server = server;
      sessions.set(session.id, session);
      await server.connect(transport);
      if (session.closed) {
        return;
      }
      clearTimeout(session.setupTimer);
      startSessionTimers(session);
    } catch (error) {
      console.error('Error establishing SSE connection:', error?.message);
      closeSession(session, { status: 500, error: 'Failed to establish SSE connection' });
    }
  }

  app.get('/mcp', (req, res) => {
    // Express routes HEAD here too; a HEAD response can never carry the
    // stream, so refuse it instead of admitting a session it can't use.
    if (req.method === 'HEAD') {
      return sendJson(res, 405, { error: 'Method not allowed' }, { Allow: 'GET, POST' });
    }
    if (shuttingDown) {
      return sendJson(res, 503, { error: 'Server is shutting down' });
    }
    const pending = liveSessions.size - sessions.size;
    if (pending >= limits.maxPendingSessions || liveSessions.size >= limits.maxActiveSessions) {
      return sendJson(res, 429, { error: 'Too many SSE sessions' }, { 'Retry-After': '1' });
    }
    return openSession(req, res);
  });

  function refuseSession(res) {
    sendJson(res, 400, { error: 'Invalid or missing session ID' });
  }

  async function dispatchMessage(session, req, res) {
    const message = req.body;
    const cancelledKey = cancelledRequestKey(message);
    if (cancelledKey !== null && session.outstanding.has(cancelledKey)) {
      // Tool handlers don't observe abort signals, so forwarding the
      // cancellation would only make the SDK drop the eventual response
      // while the handler keeps working. Keep the request's slot until the
      // handler settles and discard its late response instead, which is
      // what the client expects after cancelling.
      session.cancelled.add(cancelledKey);
      if (isWritable(res)) {
        res.writeHead(202).end('Accepted');
      }
      return undefined;
    }
    const requestKey = jsonRpcRequestKey(message);
    if (requestKey !== null) {
      if (session.outstanding.has(requestKey)) {
        return sendJson(res, 400, { error: 'Duplicate request id' });
      }
      if (
        session.outstanding.size >= limits.maxOutstandingRequestsPerSession
        || outstandingRequests >= limits.maxOutstandingRequests
      ) {
        return sendJson(res, 429, { error: 'Too many outstanding requests' });
      }
      session.outstanding.add(requestKey);
      outstandingRequests += 1;
    }
    try {
      await session.transport.handlePostMessage(req, res, message);
    } finally {
      if (requestKey !== null && res.statusCode !== 202) {
        releaseOutstanding(session, requestKey);
      }
    }
    return undefined;
  }

  async function processPost(session, req, res) {
    try {
      await runMiddleware(parseJson, req, res);
    } catch (error) {
      if (error?.status === 413) {
        return sendJson(res, 413, { error: 'Request body too large' });
      }
      if (error?.status === 415) {
        return sendJson(res, 415, { error: 'Unsupported request body encoding' });
      }
      return sendJson(res, 400, { error: 'Invalid JSON body' });
    }
    if (req.body === undefined) {
      return sendJson(res, 400, { error: 'Invalid JSON body' });
    }
    if (session.closed) {
      return refuseSession(res);
    }
    return dispatchMessage(session, req, res);
  }

  app.post('/mcp', async (req, res) => {
    const { sessionId } = req.query;
    const session = typeof sessionId === 'string' ? sessions.get(sessionId) : undefined;
    if (!session || session.closed) {
      return refuseSession(res);
    }
    if (!req.is('application/json')) {
      return sendJson(res, 415, { error: 'Content-Type must be application/json' });
    }

    const sessionTurn = session.posts.acquire();
    if (!sessionTurn) {
      return sendJson(res, 429, { error: 'Too many queued requests for this session' });
    }
    if (!(await sessionTurn)) {
      return refuseSession(res);
    }
    try {
      const globalTurn = postSlots.acquire();
      if (!globalTurn) {
        return sendJson(res, 429, { error: 'Too many concurrent requests' });
      }
      await globalTurn;
      try {
        if (session.closed) {
          return refuseSession(res);
        }
        if (!isWritable(res)) {
          return undefined;
        }
        markActivity(session);
        return await processPost(session, req, res);
      } finally {
        postSlots.release();
      }
    } catch (error) {
      console.error('Error handling POST message:', error?.message);
      return sendJson(res, 500, { error: 'Failed to process message' });
    } finally {
      session.posts.release();
    }
  });

  app.get('/health', (req, res) => {
    res.json({
      status: 'healthy',
      servicenow_instance: defaultInstance.url,
      instance_name: defaultInstance.name,
      timestamp: new Date().toISOString()
    });
  });

  app.get('/instances', (req, res) => {
    res.json({ instances: listInstances() });
  });

  // Final error boundary: never echo request bodies, stacks or headers.
  app.use((error, req, res, next) => {
    if (res.headersSent) {
      return next(error);
    }
    console.error('HTTP request failed:', error?.message);
    return sendJson(res, 500, { error: 'Internal server error' });
  });

  app.closeAllSessions = async () => {
    shuttingDown = true;
    await Promise.all([...liveSessions].map(
      (session) => closeSession(session, { status: 503, error: 'Server is shutting down' })
    ));
  };

  return app;
}
