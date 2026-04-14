#!/usr/bin/env node

/**
 * Happy MCP Server - Stdio Transport
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 */

import dotenv from 'dotenv';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { ServiceNowClient } from './servicenow-client.js';
import { createMcpServer } from './mcp-server-consolidated.js';
import { configManager } from './config-manager.js';

// Load environment variables
dotenv.config();

async function main() {
  try {
    // Get instance configuration (from SERVICENOW_INSTANCE env var or default)
    const instance = configManager.getInstanceOrDefault(process.env.SERVICENOW_INSTANCE);

    console.error(`🔗 Default ServiceNow instance: ${instance.name} (${instance.url})`);
    console.error(`💡 Use SN-Set-Instance tool to switch instances during session`);

    // Create ServiceNow client
    const serviceNowClient = new ServiceNowClient(
      instance.url,
      instance.username,
      instance.password,
      {
        authType: instance.authType || 'basic',
        clientId: instance.clientId,
        clientSecret: instance.clientSecret,
        grantType: instance.grantType,
        scope: instance.scope
      }
    );
    serviceNowClient.currentInstanceName = instance.name;

    // Create MCP server with all tools
    const server = await createMcpServer(serviceNowClient);

    // Create stdio transport
    const transport = new StdioServerTransport();

    // Connect server to transport
    await server.connect(transport);

    console.error('Happy MCP Server (stdio) started successfully');
    console.error(`Instance: ${instance.name} - ${instance.url}`);
  } catch (error) {
    console.error('Failed to start Happy MCP Server:', error);
    process.exit(1);
  }
}

main();