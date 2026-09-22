import { timingSafeEqual } from 'node:crypto';
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

const LOOPBACK_HOSTS = new Set(['127.0.0.1', '::1', 'localhost']);

function isLoopbackRemote(req) {
  const remote = (req.socket.remoteAddress || '').replace(/^::ffff:/, '');
  return LOOPBACK_HOSTS.has(remote);
}

export function validateHttpTransportSecurity({ host = '127.0.0.1', apiToken } = {}) {
  if (!LOOPBACK_HOSTS.has(host) && !apiToken) {
    throw new Error('HAPPY_MCP_API_TOKEN is required when HAPPY_MCP_BIND_HOST is not loopback');
  }
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

export function createHttpApp({
  defaultInstance,
  apiToken,
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
  const app = express();
  const sessions = new Map();
  app.use(express.json());

  if (apiToken) {
    const expectedToken = Buffer.from(apiToken);
    app.use((req, res, next) => {
      const match = /^Bearer (.+)$/.exec(req.get('authorization') || '');
      const suppliedToken = match ? Buffer.from(match[1]) : null;
      if (!suppliedToken || suppliedToken.length !== expectedToken.length ||
        !timingSafeEqual(suppliedToken, expectedToken)) {
        return res.status(401).json({ error: 'Unauthorized' });
      }
      next();
    });
  }

  app.get('/mcp', async (req, res) => {
    let keepaliveInterval;
    try {
      res.setHeader('Cache-Control', 'no-cache, no-transform');
      res.setHeader('X-Accel-Buffering', 'no');
      res.setHeader('Connection', 'keep-alive');
      const transport = new Transport('/mcp', res);
      const serviceNowClient = createServiceNowClient(defaultInstance, { credentialStore });
      const server = await createServer(serviceNowClient, {
        configManager,
        instanceRegistry,
        credentialStore
      });
      const cleanup = () => {
        clearInterval(keepaliveInterval);
        sessions.delete(transport.sessionId);
      };

      keepaliveInterval = setInterval(() => {
        res.write(': keepalive\n\n');
      }, keepaliveIntervalMs);
      transport.onclose = cleanup;
      req.on('close', cleanup);
      req.on('error', cleanup);

      sessions.set(transport.sessionId, { server, transport });
      await server.connect(transport);
    } catch (error) {
      console.error('Error establishing SSE connection:', error.message);
      clearInterval(keepaliveInterval);
      if (!res.headersSent) {
        res.status(500).json({ error: 'Failed to establish SSE connection' });
      }
    }
  });

  app.post('/mcp', async (req, res) => {
    try {
      const session = sessions.get(req.query.sessionId);
      if (!session) {
        return res.status(400).json({ error: 'Invalid or missing session ID' });
      }
      await session.transport.handlePostMessage(req, res, req.body);
    } catch (error) {
      console.error('Error handling POST message:', error.message);
      res.status(500).json({ error: 'Failed to process message' });
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
    if (!apiToken && !isLoopbackRemote(req)) {
      return res.status(401).json({ error: 'Unauthorized' });
    }
    res.json({ instances: listInstances() });
  });

  return app;
}
