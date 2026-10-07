# ServiceNow MCP Server Setup Guide

## Two Server Modes

This MCP server can run in two different modes:

### 1. HTTP/SSE Mode (Port 3000) - For Claude Code & Testing
- **File**: `src/server.js`
- **Port**: 3000 (configurable via PORT env var)
- **Usage**: Claude Code integration, API testing, web-based access, for **one trusted operator**
- **Start Command**: `npm start` or `npm run dev` (requires `HAPPY_MCP_API_TOKEN`)
- **Endpoint**: http://localhost:3000/mcp (every request needs `Authorization: Bearer <token>`)

### 2. STDIO Mode - For Claude Desktop App
- **File**: `src/stdio-server.js`
- **Usage**: Claude Desktop app integration
- **Start Command**: `npm start:stdio`
- **No port required** (uses standard input/output)

## Configuration for Claude Desktop

Add to your Claude Desktop configuration (`~/Library/Application Support/Claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "servicenow": {
      "command": "node",
      "args": ["/absolute/path/to/happy-platform-mcp/src/stdio-server.js"],
      "env": {
        "SERVICENOW_INSTANCE_URL": "https://your-instance.service-now.com",
        "SERVICENOW_USERNAME": "your-username",
        "SERVICENOW_PASSWORD": "your-password"
      }
    }
  }
}
```

## Configuration for Claude Code

The HTTP server runs on port 3000 and refuses to start without a bearer token:
```bash
export HAPPY_MCP_API_TOKEN="$(openssl rand -hex 32)"
npm start
# or
npm run dev
```

Configure your MCP client to send `Authorization: Bearer <token>` on both the SSE `GET /mcp` request and the message `POST` requests.

## Running Both Simultaneously

You can run both servers at the same time:

1. **Terminal 1** - HTTP Server for Claude Code:
```bash
npm start
```

2. **Claude Desktop** - Will automatically start stdio server when needed

## Testing the Servers

### Test HTTP Server:
```bash
# Health check (401 without the bearer token)
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/health

# Test MCP endpoint (prints the endpoint event, then keepalive comments)
curl -N -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/mcp
```

### Test STDIO Server:
```bash
# Run directly to see output
node src/stdio-server.js
# Press Ctrl+C to exit
```

## Common Issues

1. **Port 3000 already in use**: Kill existing process:
```bash
pkill -f "node src/server.js"
```

2. **Claude Desktop not connecting**:
- Restart Claude Desktop after updating config
- Check the path in config matches your actual path
- Ensure credentials in config are correct

3. **Both trying to use same port**:
- HTTP server uses port 3000
- STDIO server doesn't use any port
- They can run simultaneously without conflict

## Environment Variables

Make sure your `.env` file contains:
```
SERVICENOW_INSTANCE_URL=https://your-instance.service-now.com
SERVICENOW_USERNAME=admin
SERVICENOW_PASSWORD=your-password
PORT=3000
DEBUG=true
```

### HTTP transport security

The HTTP/SSE transport serves a single trusted operator; shared multi-user hosting is unsupported. Every HTTP listener, including loopback, requires `HAPPY_MCP_API_TOKEN`: 32 random bytes as 64 hex characters (or 43 base64url). Startup fails if it's missing or malformed. Stdio mode doesn't use it.

```bash
# Generate once and keep it out of version control
openssl rand -hex 32
```

```
HAPPY_MCP_API_TOKEN=<64 hex characters from openssl rand -hex 32>
# Optional: listen beyond loopback (prefer a TLS reverse proxy)
HAPPY_MCP_BIND_HOST=127.0.0.1
# Optional: extra Host authorities, e.g. the public name your reverse proxy forwards
HAPPY_MCP_ALLOWED_HOSTS=mcp.example.com
# Optional: exact browser origins; unset rejects any request carrying Origin
HAPPY_MCP_ALLOWED_ORIGINS=https://console.example.com
```

`/health`, `/instances` and `/mcp` all require the bearer token. Requests whose `Host` doesn't name the listener or an approved authority, or whose `Origin` isn't approved, get `403`. Ten failed attempts from one address within a minute trigger `429`. See [HTTP Transport Security](../README.md#http-transport-security) for limits, reverse-proxy setup and client migration.

### OAuth Environment Variables (Optional)

For OAuth authentication via `.env` (single-instance fallback), add:
```
SERVICENOW_AUTH_TYPE=oauth
SERVICENOW_CLIENT_ID=your-oauth-client-id
SERVICENOW_CLIENT_SECRET=your-oauth-client-secret
```

For per-user Authorization Code with PKCE, configure a ServiceNow public client and add:
```
SERVICENOW_AUTH_TYPE=oauth
SERVICENOW_OAUTH_GRANT_TYPE=authorization_code
SERVICENOW_CLIENT_ID=your-public-client-id
SERVICENOW_OAUTH_AUTHORIZE_URL=https://your-instance.service-now.com/oauth_auth.do
SERVICENOW_OAUTH_TOKEN_URL=https://your-instance.service-now.com/oauth_token.do
SERVICENOW_OAUTH_REDIRECT_PORT=8202
SERVICENOW_OAUTH_CALLBACK_PATH=/callback
```

Register `http://127.0.0.1:8202/callback` as the public client's redirect URL. Do not set `SERVICENOW_CLIENT_SECRET` for a public client.

For multi-instance setups, configure OAuth per-instance in `config/servicenow-instances.json` instead. See [Multi-Instance Configuration](MULTI_INSTANCE_CONFIGURATION.md#oauth-authentication).