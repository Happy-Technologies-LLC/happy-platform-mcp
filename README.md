<p align="center">
  <img src="https://happy-tech.biz/images/logo.svg" alt="Happy MCP Server" width="120" height="120">
</p>

<h1 align="center">Happy MCP Server</h1>

<p align="center">
  <strong>Model Context Protocol Server for the ServiceNow&reg; Platform</strong></p>

<p align="center">
  A metadata-driven MCP server that auto-generates 480+ tools across 160+ tables, with multi-instance support, natural language search, and local script development.
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/happy-platform-mcp"><img src="https://img.shields.io/npm/v/happy-platform-mcp.svg?style=flat-square" alt="npm version"></a>
  <a href="https://opensource.org/licenses/Apache-2.0"><img src="https://img.shields.io/badge/License-Apache%202.0-blue.svg?style=flat-square" alt="License: Apache 2.0"></a>
  <a href="https://nodejs.org"><img src="https://img.shields.io/badge/node-%3E%3D18-brightgreen.svg?style=flat-square" alt="Node.js Version"></a>
</p>

<p align="center">
  <a href="https://happy-tech.biz">Website</a> |
  <a href="https://github.com/Happy-Technologies-LLC/happy-platform-mcp">GitHub</a> |
  <a href="https://www.npmjs.com/package/happy-platform-mcp">npm</a> |
  <a href="#tool-overview">Tools</a> |
  <a href="CONTRIBUTING.md">Contributing</a>
</p>

---

> **Migrating from `servicenow-mcp-server`?** The npm package has been renamed to `happy-platform-mcp` and the Docker image to `nczitzer/happy-platform-mcp`. The old names are deprecated but will continue to work temporarily. Update your dependencies:
> ```bash
> # npm
> npm uninstall servicenow-mcp-server && npm install happy-platform-mcp
>
> # Docker
> docker pull nczitzer/happy-platform-mcp:latest
> ```

## Features

- **Multi-Instance Support** — Connect to multiple ServiceNow&reg; instances simultaneously with per-request routing
- **OAuth 2.0 & Basic Auth** — Per-instance authentication with Resource Owner Password Credentials grant, automatic token refresh, and seamless fallback
- **Intelligent Schema Discovery** — Automatically discovers table structures and relationships at runtime
- **160+ Tables** — Complete coverage including ITSM, CMDB, Service Catalog, Platform Development, and Flow Designer
- **44 MCP Tools** — Generic CRUD operations that work on any table, plus specialized convenience tools
- **Batch Operations** — 43+ parallel operations tested successfully
- **Local Script Development** — Sync scripts with Git, watch mode for continuous development
- **Natural Language Search** — Query using plain English instead of encoded queries
- **MCP Resources** — 8 read-only resource URIs for quick lookups and documentation
- **Background Script Execution** — Automated server-side script execution via `sys_trigger`

## Quick Start

### Prerequisites

- Node.js 18+
- One or more ServiceNow&reg; instances with REST API access
- Valid credentials for each instance

### Install from npm

```bash
npx happy-platform-mcp
```

Or install globally:

```bash
npm install -g happy-platform-mcp
```

### Install from Source

```bash
git clone https://github.com/Happy-Technologies-LLC/happy-platform-mcp.git
cd happy-platform-mcp
npm install
```

### Configure Instances

**Option A: Multi-Instance (Recommended)**

```bash
cp config/servicenow-instances.json.example config/servicenow-instances.json
```

Edit `config/servicenow-instances.json`:

```json
{
  "instances": [
    {
      "name": "dev",
      "url": "https://dev123456.service-now.com",
      "username": "admin",
      "password": "your-password",
      "default": true
    },
    {
      "name": "prod",
      "url": "https://prod789012.service-now.com",
      "authType": "oauth",
      "grantType": "client_credentials",
      "clientId": "your-oauth-client-id",
      "clientSecret": "your-oauth-client-secret"
    }
  ]
}
```

Each instance can use `"authType": "basic"` (default) or `"authType": "oauth"`. OAuth instances require `clientId` and `clientSecret` from your ServiceNow OAuth Application Registry. See [Authentication](#authentication) for details.

**Option B: Single Instance (via Environment)**

```bash
cp .env.example .env
# Edit .env with your credentials
```

### Start the Server

```bash
# HTTP/SSE transport
npm run dev

# Stdio transport (for Claude Desktop)
npm run stdio
```

### Verify

```bash
curl http://localhost:3000/health
curl http://localhost:3000/instances
```

## Multi-Instance Routing

All tools accept an optional `instance` parameter:

```javascript
// Uses default instance
SN-List-Incidents({ "limit": 10 })

// Routes to a specific instance
SN-List-Incidents({ "instance": "prod", "limit": 10 })
```

## Tool Overview

| Category | Tools | Description |
|----------|-------|-------------|
| **Generic CRUD** | 7 | Query, Create, Get, Update on any table |
| **Specialized ITSM** | 8 | Incident, Change, Problem convenience wrappers |
| **Convenience** | 10 | Add-Comment, Add-Work-Notes, Assign, Resolve, Close |
| **Natural Language** | 1 | Query using plain English |
| **Update Sets** | 6 | Set, list, move, clone, inspect update sets |
| **Scripts** | 2 | Execute background scripts, create fix scripts |
| **Script Sync** | 3 | Sync scripts with local files, watch mode |
| **Workflows** | 4 | Create workflows, activities, transitions |
| **Batch** | 2 | Batch create/update across tables |
| **Schema** | 3 | Table schemas, field info, relationships |
| **Resources** | 8 | Read-only URIs for table lists, field info |

### Examples

```javascript
// Query with filtering
SN-Query-Table({ "table_name": "incident", "query": "active=true^priority=1", "limit": 10 })

// Create a record
SN-Create-Incident({ "short_description": "Email service down", "urgency": 1 })

// Natural language search
SN-NL-Search({ "table_name": "incident", "query": "high priority incidents assigned to me" })

// Background script execution (automated via sys_trigger)
SN-Execute-Background-Script({ "script": "gs.info('Hello');" })

// Update set management
SN-Set-Update-Set({ "update_set_sys_id": "abc123..." })

// Batch operations
SN-Batch-Update({ "updates": [{ "table": "incident", "sys_id": "id1", "data": {...} }] })
```

### Local Script Development

Develop scripts locally with version control and automatic sync:

```javascript
// Download script to local file
SN-Sync-Script-To-Local({
  "script_sys_id": "abc123...",
  "local_path": "/scripts/business_rules/validate_incident.js"
})

// Watch for changes and auto-sync
SN-Watch-Script({
  "local_path": "/scripts/business_rules/validate_incident.js",
  "script_sys_id": "abc123..."
})
```

### Natural Language Search

```javascript
SN-NL-Search({
  "table_name": "incident",
  "query": "active high priority incidents that are unassigned"
})
```

Supports 15+ patterns including field comparisons, text searches, date ranges, logical operators, and ordering.

## Claude Desktop Integration

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

**Basic Auth:**

```json
{
  "mcpServers": {
    "happy-mcp-server": {
      "command": "npx",
      "args": ["-y", "happy-platform-mcp"],
      "env": {
        "SERVICENOW_INSTANCE_URL": "https://your-instance.service-now.com",
        "SERVICENOW_USERNAME": "your-username",
        "SERVICENOW_PASSWORD": "your-password"
      }
    }
  }
}
```

**OAuth:**

```json
{
  "mcpServers": {
    "happy-mcp-server": {
      "command": "npx",
      "args": ["-y", "happy-platform-mcp"],
      "env": {
        "SERVICENOW_INSTANCE_URL": "https://your-instance.service-now.com",
        "SERVICENOW_USERNAME": "your-username",
        "SERVICENOW_PASSWORD": "your-password",
        "SERVICENOW_AUTH_TYPE": "oauth",
        "SERVICENOW_CLIENT_ID": "your-client-id",
        "SERVICENOW_CLIENT_SECRET": "your-client-secret"
      }
    }
  }
}
```

Or if installed from source, use `"command": "node"` with `"args": ["/path/to/happy-platform-mcp/src/stdio-server.js"]` and `"cwd": "/path/to/happy-platform-mcp"`.

For multi-instance configurations, use `config/servicenow-instances.json` instead of environment variables. See [Configure Instances](#configure-instances).

Restart Claude Desktop after editing the config.

## Authentication

Happy MCP Server supports two authentication methods per instance. Both can coexist — instance A can use basic auth while instance B uses OAuth.

### Basic Auth (Default)

No extra configuration needed. Provide `username` and `password`:

```json
{
  "name": "dev",
  "url": "https://dev123456.service-now.com",
  "username": "admin",
  "password": "your-password",
  "default": true
}
```

### OAuth 2.0

Supports both **Client Credentials** (recommended) and **Resource Owner Password Credentials** grant types. Tokens are automatically requested, cached, and refreshed.

**Client Credentials (recommended)** — no user credentials needed, ideal for service-to-service integrations and federated identity environments:

```json
{
  "name": "prod",
  "url": "https://prod789012.service-now.com",
  "authType": "oauth",
  "grantType": "client_credentials",
  "clientId": "your-oauth-client-id",
  "clientSecret": "your-oauth-client-secret"
}
```

**Resource Owner Password Credentials** — for cases where user context is required:

```json
{
  "name": "staging",
  "url": "https://staging.service-now.com",
  "authType": "oauth",
  "grantType": "password",
  "clientId": "your-oauth-client-id",
  "clientSecret": "your-oauth-client-secret",
  "username": "integration_user",
  "password": "your-password"
}
```

If `grantType` is omitted, it defaults to `client_credentials` when no username is provided, or `password` when username is present.

**ServiceNow setup:**

1. Navigate to **System OAuth > Application Registry**
2. Click **New** and select **Create an OAuth API endpoint for external clients**
3. Set a name (e.g., "MCP Server") and note the generated **Client ID** and **Client Secret**
4. Add those values to your instance configuration

**How it works:**

- On first API call, requests an access token from `/oauth_token.do`
- Caches the token and automatically refreshes it before expiry (30-second buffer)
- On 401 responses, transparently refreshes the token and retries the request once
- Falls back to a fresh token grant if the refresh token is expired

The `scope` field is optional and defaults to ServiceNow's standard scope.

## Architecture

```
src/
├── server.js                     # Express HTTP server (SSE transport)
├── stdio-server.js               # Stdio transport (Claude Desktop)
├── mcp-server-consolidated.js    # MCP tool registration & routing
├── servicenow-client.js          # REST API client
└── config-manager.js             # Multi-instance configuration

config/
└── servicenow-instances.json     # Instance configuration

docs/
├── API_REFERENCE.md              # Complete tool reference
├── SETUP_GUIDE.md                # Detailed setup instructions
└── research/                     # Technical research & discoveries
```

## Testing

```bash
# Run tests
npm test

# Watch mode
npm run test:watch

# Coverage
npm run test:coverage

# MCP Inspector
npm run inspector
```

## Troubleshooting

### Connection Issues

```bash
# Test connectivity to your ServiceNow instance
curl -u username:password https://your-instance.service-now.com/api/now/table/incident?sysparm_limit=1

# Check server health
curl http://localhost:3000/health
```

### Common Problems

- **Multi-instance not working:** Verify `config/servicenow-instances.json` is valid JSON with one `"default": true` instance. Restart after changes.
- **Tools not appearing:** Check MCP Inspector connection and server logs.
- **Auth failures:** Test credentials in browser first. Ensure the user has required roles.
- **SSE disconnects in Docker:** Enable keepalive (default 15s). See `docs/SSE_SETUP_GUIDE.md`.

### Debug Mode

```bash
DEBUG=true npm run dev
```

## Known Limitations

- Flow Designer logic blocks cannot be created via REST API (use the UI)
- Flow compilation/validation must be done in the UI
- UI Policy Actions linking requires a background script workaround

See `docs/MCP_Tool_Limitations.md` for details.

## Acknowledgments

This project was inspired by the [Echelon AI Labs ServiceNow MCP Server](https://github.com/echelon-ai-labs/servicenow-mcp). We are grateful for their pioneering work in bringing MCP capabilities to the ServiceNow&reg; platform.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines. All contributors must sign a CLA.

## Security

To report a vulnerability, see [SECURITY.md](SECURITY.md). Do not open public issues for security concerns.

## License

Licensed under the [Apache License 2.0](LICENSE).

Copyright 2025 Happy Technologies LLC

---

## Trademark Notice

ServiceNow&reg; is a registered trademark of ServiceNow, Inc. "Now" is a registered trademark of ServiceNow, Inc. All ServiceNow&reg; product names, logos, and brands are property of ServiceNow, Inc.

Model Context Protocol (MCP) is an open standard created by Anthropic, PBC. "Claude" is a trademark of Anthropic, PBC.

**Happy MCP Server is an independent, community-driven project.** It is not affiliated with, endorsed by, or sponsored by ServiceNow, Inc. or Anthropic, PBC. This project provides tooling that connects to ServiceNow&reg; instances via their published REST APIs, and implements the open MCP specification. It is not a competitor to any ServiceNow&reg; product or service.

All other trademarks are the property of their respective owners. See [NOTICE](NOTICE) for full attribution.
