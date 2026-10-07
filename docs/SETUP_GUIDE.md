# ServiceNow MCP Server Setup Guide

## Two Server Modes

This MCP server can run in two different modes:

### 1. HTTP/SSE Mode (Port 3000) - For Claude Code & Testing
- **File**: `src/server.js`
- **Port**: 3000 (configurable via PORT env var)
- **Usage**: Claude Code integration, API testing, web-based access
- **Start Command**: `npm start:http` or `npm run dev`
- **Endpoint**: http://localhost:3000/mcp

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

The HTTP server runs automatically on port 3000 when you use:
```bash
npm start
# or
npm run dev
```

## Running Both Simultaneously

You can run both servers at the same time:

1. **Terminal 1** - HTTP Server for Claude Code:
```bash
npm start:http
```

2. **Claude Desktop** - Will automatically start stdio server when needed

## Testing the Servers

### Test HTTP Server:
```bash
# Health check
curl http://localhost:3000/health

# Test MCP endpoint
curl -X GET http://localhost:3000/mcp
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

HTTP/SSE binds to `127.0.0.1` by default. For a non-loopback `HAPPY_MCP_BIND_HOST`, set `HAPPY_MCP_API_TOKEN` and require clients to send it as a bearer token:
```
HAPPY_MCP_BIND_HOST=0.0.0.0
HAPPY_MCP_API_TOKEN=replace-with-a-high-entropy-secret
```

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

### Refresh-token storage

Authorization-code refresh tokens default to the OS keychain. Leave
`SERVICENOW_TOKEN_STORE` unset or set it to `keychain` (also the supported Windows
choice). An unavailable keychain fails rather than silently falling back to files.

On a trusted POSIX filesystem, explicitly opt in by adding this to the server
process environment or its private `.env` file:

```sh
SERVICENOW_TOKEN_STORE=file
```

Tokens are plaintext files at `$XDG_CONFIG_HOME/happy-platform-mcp/token-<sha256-hex>`
or `~/.config/happy-platform-mcp/token-<sha256-hex>` if XDG is unset, where the name is
the lowercase SHA-256 hex of the account key. Keys differing only by case therefore
never share a file on case-insensitive filesystems such as default macOS APFS. Older
`token-<account>` files are not read or migrated; authorize again (you may delete them).
If overriding XDG, use an absolute trusted directory. New storage is created private;
existing storage must already be owned by the current OS user with mode 0700, and
existing token files must be regular, singly linked, current-user-owned files with
mode 0600. Before creating any directory, every existing ancestor is resolved
(symlinked ancestors are followed) and checked: each must be a directory owned by the
current user or root and not group/world-writable unless sticky (like `/tmp`). The
error names the offending path; for example, a group-writable `~/.config` from umask
002 needs `chmod go-w ~/.config`. Missing intermediate directories are created one at
a time and re-checked. Unsafe symlinks, types, ownership and permissions are rejected;
permission/setup failures are not ignored. Inspect any existing directory and its
contents before manually making it private. Do not select file storage on
shared/network filesystems with untrusted ownership or ACL policies.
Windows is rejected because POSIX chmod cannot establish private Windows ACLs.

This opt-in trades keychain isolation for access by every process of the same OS
user. Exclude token storage from shared/cloud backups and version control, or
encrypt and restrict backups as secrets. No token migration occurs when switching
stores; authorize again. Instance passwords/client secrets still use the keychain.

Unique exclusive temporary files (0600) and atomic same-directory rename protect
complete file replacement during concurrent writes, not cross-process OAuth
refresh-token rotation or power-loss durability. Run only one refreshing process
per identity; no distributed locking or refresh-rotation guarantee is provided.
