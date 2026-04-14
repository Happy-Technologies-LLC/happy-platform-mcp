# Multi-Instance Configuration Guide

## Overview

The ServiceNow MCP Server now supports multiple instance configurations through a centralized JSON file. This allows you to manage credentials for dev, test, and production instances in one place and switch between them easily.

## Configuration File

Create `config/servicenow-instances.json` with your instance credentials:

```json
{
  "instances": [
    {
      "name": "dev",
      "url": "https://dev276360.service-now.com",
      "username": "admin",
      "password": "your_password",
      "default": true,
      "description": "Development instance"
    },
    {
      "name": "prod",
      "url": "https://yourinstance.service-now.com",
      "authType": "oauth",
      "grantType": "client_credentials",
      "clientId": "your_oauth_client_id",
      "clientSecret": "your_oauth_client_secret",
      "default": false,
      "description": "Production instance (OAuth)"
    }
  ]
}
```

### Instance Configuration Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | Yes | — | Unique instance identifier |
| `url` | Yes | — | ServiceNow instance URL |
| `username` | Yes | — | Username for authentication |
| `password` | Yes | — | Password for authentication |
| `authType` | No | `"basic"` | `"basic"` or `"oauth"` |
| `grantType` | No | auto | `"client_credentials"` or `"password"` (auto-detected if omitted) |
| `clientId` | OAuth only | — | OAuth Client ID from Application Registry |
| `clientSecret` | OAuth only | — | OAuth Client Secret |
| `scope` | No | — | OAuth scope (optional) |
| `default` | No | `false` | Mark as default instance |
| `description` | No | — | Human-readable description |

**Important:** The `config/servicenow-instances.json` file is gitignored to prevent committing credentials.

## Instance Selection

### 1. Default Instance (HTTP Server)
The HTTP server (`src/server.js`) uses the instance marked with `"default": true`.

```bash
npm start
# Uses the default instance from config
```

### 2. Environment Variable (stdio Server)
For the stdio server used by Claude Desktop, set the `SERVICENOW_INSTANCE` environment variable:

```bash
SERVICENOW_INSTANCE=prod node src/stdio-server.js
# Uses the "prod" instance
```

In your Claude Desktop configuration:

```json
{
  "mcpServers": {
    "servicenow": {
      "command": "node",
      "args": ["src/stdio-server.js"],
      "cwd": "/path/to/mcp-servicenow-nodejs",
      "env": {
        "SERVICENOW_INSTANCE": "dev"
      }
    }
  }
}
```

### 3. Backward Compatibility (.env)
If `config/servicenow-instances.json` doesn't exist, the system falls back to `.env`:

```env
SERVICENOW_INSTANCE_URL=https://dev276360.service-now.com
SERVICENOW_USERNAME=admin
SERVICENOW_PASSWORD=your_password
```

## API Endpoints

### List Available Instances
```bash
curl http://localhost:3000/instances
```

Response:
```json
{
  "instances": [
    {
      "name": "dev",
      "url": "https://dev276360.service-now.com",
      "default": true,
      "description": "Development instance"
    },
    {
      "name": "prod",
      "url": "https://yourinstance.service-now.com",
      "default": false,
      "description": "Production instance"
    }
  ]
}
```

### Health Check
```bash
curl http://localhost:3000/health
```

Response:
```json
{
  "status": "healthy",
  "servicenow_instance": "https://dev276360.service-now.com",
  "instance_name": "dev",
  "timestamp": "2025-09-30T12:46:25.330Z"
}
```

## Configuration Manager API

The `ConfigManager` class (src/config-manager.js) provides:

### Methods

- `loadInstances()` - Load all instances from JSON or .env
- `getInstance(name)` - Get specific instance by name
- `getDefaultInstance()` - Get the default instance
- `getInstanceOrDefault(name)` - Get instance by name or default
- `listInstances()` - List all instances (without passwords)
- `validateInstance(instance)` - Validate instance configuration

### Example Usage

```javascript
import { configManager } from './config-manager.js';

// Get default instance
const instance = configManager.getDefaultInstance();

// Get specific instance
const prodInstance = configManager.getInstance('prod');

// Get instance or default
const instance = configManager.getInstanceOrDefault(process.env.SERVICENOW_INSTANCE);

// List all instances
const instances = configManager.listInstances();
```

## Migration from .env

1. Create `config/servicenow-instances.json` using the example template
2. Copy your credentials from `.env` to the JSON file
3. Test with `npm start` - should load from JSON
4. Optionally remove ServiceNow credentials from `.env` (keep other vars)

## OAuth Authentication

Each instance can independently use basic auth or OAuth 2.0. Two OAuth grant types are supported:

- **Client Credentials** (recommended) — service-to-service, no user credentials needed. Works with federated identity environments.
- **Resource Owner Password Credentials** — when user context is required. Requires username/password.

### ServiceNow Setup

1. Navigate to **System OAuth > Application Registry**
2. Click **New** > **Create an OAuth API endpoint for external clients**
3. Set a name and note the **Client ID** and **Client Secret**
4. Add those values to your instance config with `"authType": "oauth"`

### Token Lifecycle

- Tokens are requested automatically on the first API call via `/oauth_token.do`
- Cached in memory and refreshed before expiry (30-second buffer)
- On 401 response, the token is refreshed and the request retried once
- If the refresh token is expired, a fresh token grant is issued

### Mixing Auth Types

You can freely mix basic auth, client credentials, and password grant instances:

```json
{
  "instances": [
    {
      "name": "dev",
      "url": "https://dev123.service-now.com",
      "username": "admin",
      "password": "password",
      "default": true
    },
    {
      "name": "prod",
      "url": "https://prod456.service-now.com",
      "authType": "oauth",
      "grantType": "client_credentials",
      "clientId": "abc...",
      "clientSecret": "xyz..."
    },
    {
      "name": "staging",
      "url": "https://staging789.service-now.com",
      "authType": "oauth",
      "grantType": "password",
      "clientId": "abc...",
      "clientSecret": "xyz...",
      "username": "integration_user",
      "password": "password"
    }
  ]
}
```

If `grantType` is omitted, it defaults to `client_credentials` when no username is provided, or `password` when username is present.

When switching instances via `SN-Set-Instance`, the server automatically uses the correct auth method for the target instance.

## Security Notes

- **Never commit** `config/servicenow-instances.json` (already gitignored)
- Keep the `.example` file without real credentials for documentation
- Use environment-specific passwords
- Consider using secrets management tools for production

## Troubleshooting

### "Instance not found" error
```
Instance 'staging' not found. Available instances: dev, prod, test
```
**Solution:** Check instance name spelling or add the instance to config file.

### Falls back to .env
```
⚠️  servicenow-instances.json not found, falling back to .env
```
**Solution:** Create `config/servicenow-instances.json` from the example file.

### Missing credentials
```
Missing ServiceNow credentials. Create config/servicenow-instances.json...
```
**Solution:** Either create JSON config or set env vars in `.env`.