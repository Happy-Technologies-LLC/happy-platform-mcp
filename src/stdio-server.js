#!/usr/bin/env node

/**
 * Happy MCP Server - Stdio Transport
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 */

import dotenv from 'dotenv';
import { pathToFileURL } from 'url';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { ServiceNowClient } from './servicenow-client.js';
import { createMcpServer } from './mcp-server-consolidated.js';
import { configManager as defaultConfigManager, instanceToClientOptions } from './config-manager.js';
import { InstanceCredentialStore } from './instance-credential-store.js';
import { installProcessCrashGuards } from './process-guards.js';
import { versionChangeHint } from './project-links.js';
import { defaultUsageTracker } from './usage-telemetry.js';

// Load environment variables
dotenv.config();

// Guards the whole process against a single unhandled rejection or
// uncaught exception taking down every concurrent MCP session — see
// process-guards.js for the full rationale. Registration happens once
// per process (module-scoped flag), so whichever entrypoint imports
// this module first wins the label.
installProcessCrashGuards('stdio-server');

function booleanEnv(value) {
  return ['true', '1', 'yes'].includes(String(value || '').toLowerCase());
}

function hasServiceNowEnvCredentials(env = process.env) {
  // Passwordless OAuth grants (client_credentials, authorization_code) don't set
  // USERNAME/PASSWORD — an instance URL + a passwordless grant is a real intent
  // to connect, so a config error should surface rather than fall to docs-only.
  const grant = env.SERVICENOW_OAUTH_GRANT_TYPE;
  const passwordlessGrant = grant === 'client_credentials' || grant === 'authorization_code';
  if (env.SERVICENOW_INSTANCE_URL && passwordlessGrant) {
    return true;
  }
  return Boolean(env.SERVICENOW_INSTANCE_URL && env.SERVICENOW_USERNAME && env.SERVICENOW_PASSWORD);
}

export function shouldUseDocsOnlyMode(env = process.env) {
  return booleanEnv(env.HAPPY_MCP_DOCS_ONLY);
}

function docsOnlyExplicitlyDisabled(env = process.env) {
  return String(env.HAPPY_MCP_DOCS_ONLY || '').toLowerCase() === 'false';
}

function shouldFallbackToDocsOnly(error, env = process.env) {
  return !docsOnlyExplicitlyDisabled(env) &&
    !hasServiceNowEnvCredentials(env) &&
    /Missing ServiceNow credentials|servicenow-instances\.json not found/i.test(error.message);
}

export async function createConfiguredMcpServer({
  env = process.env,
  manager,
  configManager: injectedConfigManager,
  instanceRegistry,
  registry,
  credentialStore,
  ServiceNowClientClass = ServiceNowClient,
  createServer = createMcpServer
} = {}) {
  const selectedConfigManager = injectedConfigManager || manager || defaultConfigManager;
  const selectedRegistry = instanceRegistry || registry || selectedConfigManager?.registry;
  const selectedCredentialStore = credentialStore || new InstanceCredentialStore();
  const serverDependencies = {
    configManager: selectedConfigManager,
    instanceRegistry: selectedRegistry,
    credentialStore: selectedCredentialStore
  };
  const startDocsOnly = async () => {
    console.error('📚 Starting Happy MCP in docs-only mode');
    return {
      server: await createServer(null, { docsOnly: true, ...serverDependencies }),
      docsOnly: true,
      instance: null
    };
  };

  if (shouldUseDocsOnlyMode(env)) {
    return startDocsOnly();
  }

  // Get instance configuration (from SERVICENOW_INSTANCE env var or default)
  let instance;
  try {
    instance = selectedConfigManager.getInstanceOrDefault(env.SERVICENOW_INSTANCE);
  } catch (error) {
    if (shouldFallbackToDocsOnly(error, env)) {
      return startDocsOnly();
    }
    throw error;
  }

  console.error(`🔗 Default ServiceNow instance: ${instance.name} (${instance.url})`);
  console.error(`💡 Use SN-Set-Instance tool to switch instances during session`);

  // Create ServiceNow client
  const serviceNowClient = new ServiceNowClientClass(
    instance.url,
    instance.username,
    instance.password,
    instanceToClientOptions(instance, { credentialStore: selectedCredentialStore })
  );
  serviceNowClient.currentInstanceName = instance.name;

  return {
    server: await createServer(serviceNowClient, serverDependencies),
    docsOnly: false,
    instance
  };
}

export async function main() {
  try {
    const { server, docsOnly, instance } = await createConfiguredMcpServer();

    // Create stdio transport
    const transport = new StdioServerTransport();

    // Connect server to transport
    await server.connect(transport);

    console.error('Happy MCP Server (stdio) started successfully');
    if (docsOnly) {
      console.error('Mode: docs-only');
    } else {
      console.error(`Instance: ${instance.name} - ${instance.url}`);
    }
    const hint = versionChangeHint();
    if (hint) console.error(hint);
    const usage = defaultUsageTracker();
    usage.setTransport('stdio');
    if (usage.enabled) {
      console.error(usage.describe());
    }
  } catch (error) {
    console.error('Failed to start Happy MCP Server:', error);
    process.exit(1);
  }
}

const isDirectRun = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (isDirectRun) {
  main();
}
