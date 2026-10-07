/**
 * Happy MCP Server - Express HTTP Server
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 */

import dotenv from 'dotenv';
import { configManager } from './config-manager.js';
import { createHttpApp, loadHttpSecurityConfig } from './http-server.js';
import { InstanceCredentialStore } from './instance-credential-store.js';
import { installProcessCrashGuards } from './process-guards.js';

dotenv.config();

// Guards the whole process against a single unhandled rejection or
// uncaught exception taking down every concurrent MCP session — see
// process-guards.js for the full rationale. Registration is a no-op
// here in practice: http-server.js (imported above) already installed
// the handlers under the 'http-server' label, since the module-scoped
// install flag makes registration exactly-once per process regardless
// of which entrypoint calls it first.
installProcessCrashGuards('server');

const port = Number(process.env.PORT || 3000);
const host = process.env.HAPPY_MCP_BIND_HOST || '127.0.0.1';
const credentialStore = new InstanceCredentialStore();
const keepaliveIntervalMs = Number(process.env.SSE_KEEPALIVE_INTERVAL || 15000);

function refuseStartup(error) {
  console.error(`Happy MCP Server HTTP startup refused: ${error.message}`);
  process.exit(1);
}

let securityConfig;
try {
  securityConfig = loadHttpSecurityConfig(process.env);
} catch (error) {
  refuseStartup(error);
}

const defaultInstance = configManager.getDefaultInstance();
console.log(`Default ServiceNow instance: ${defaultInstance.name} (${defaultInstance.url})`);

let app;
try {
  app = createHttpApp({
    defaultInstance,
    ...securityConfig,
    configManager,
    instanceRegistry: configManager.registry,
    credentialStore,
    keepaliveIntervalMs,
    listInstances: () => configManager.listInstances()
  });
} catch (error) {
  refuseStartup(error);
}

const httpServer = app.listen(port, host, () => {
  console.log(`Happy MCP Server listening on http://${host}:${port} (bearer authentication required)`);
  console.log(`Health check: http://${host}:${port}/health`);
  console.log(`MCP SSE endpoint: http://${host}:${port}/mcp`);
  console.log(`Available instances: http://${host}:${port}/instances`);

  if (process.env.DEBUG === 'true') {
    console.log(`Active ServiceNow instance: ${defaultInstance.name} - ${defaultInstance.url}`);
  }
});

let shuttingDown = false;
async function shutdown(signal) {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;
  console.log(`Received ${signal}; closing MCP sessions`);
  httpServer.close();
  await app.closeAllSessions();
  httpServer.closeAllConnections();
  process.exit(0);
}

process.once('SIGTERM', () => void shutdown('SIGTERM'));
process.once('SIGINT', () => void shutdown('SIGINT'));
