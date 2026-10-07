# Multi-Instance Configuration Guide

The ServiceNow MCP Server loads named instance metadata from a version 1
registry and routes operations to dev, test, or production. Credentials are
never stored in the registry created by the new CLI: only canonical
`credentialRef` values point to local OS-keychain entries.

## Configuration File

Global installs and `npx` use the user-owned registry
`<homedir>/.config/happy-platform-mcp/instances.json` on every OS, with that
OS's native path separators. There is no platform-specific resolver claim.
Set one optional `HAPPY_CONFIG_PATH` setting to point to a different metadata
registry; `~` expands to the home directory and relative paths are resolved
from the process working directory. One setting/file holds all environments.

The CLI inherits its process environment and does not auto-load `.env`.
`.env` auto-loading applies to the server/stdio process only. Export
`HAPPY_CONFIG_PATH` for CLI commands, or set it in the MCP host environment
for a server/stdio launch.

```json
{
  "version": 1,
  "docs": {
    "localIndexEnabled": false
  },
  "instances": [
    {
      "name": "dev",
      "url": "https://your-dev-instance.service-now.com",
      "authType": "basic",
      "username": "your-username",
      "credentialRef": "keychain:instance/dev/password",
      "default": true,
      "description": "Development instance"
    },
    {
      "name": "prod",
      "url": "https://your-prod-instance.service-now.com",
      "authType": "oauth",
      "grantType": "client_credentials",
      "clientId": "your-client-id",
      "credentialRef": "keychain:instance/prod/client-secret",
      "default": false,
      "description": "Production instance"
    }
  ]
}
```

Use the local CLI to create and test entries:

```bash
happy-platform-mcp instance add
happy-platform-mcp instance list
happy-platform-mcp instance update dev
happy-platform-mcp instance test dev
happy-platform-mcp instance remove dev
happy-platform-mcp instance migrate
```

For Basic, OAuth `client_credentials`, and OAuth `password` entries,
interactive `instance add` prompts for and stores every required secret exactly
once before registering the metadata. Secret prompts are masked and handled
locally by the OS keychain. `instance credential set` is not part of initial
setup: use it only to rotate or replace a credential for an already registered
instance, or to recover a missing credential after metadata-only MCP
registration. The command requires an interactive TTY and does not accept
secrets from non-TTY stdin or fall back to plaintext. MCP tools never accept
passwords or client secrets.

`update` changes metadata only; authentication changes require remove/re-add,
which prompts for replacement credentials during `instance add`. For a
password-grant instance, `credentialRef` is an object with both `password` and
`clientSecret` canonical references. Basic and `client_credentials` instances
use one string reference for the respective password or client secret.
Authorization-code instances use metadata only until the interactive browser
flow stores a refresh token locally.

### Instance Configuration Fields

| Field | Required | Description |
|-------|----------|-------------|
| `version` | Yes | Registry document version; current value is `1` |
| `docs` | No | Preserved top-level docs settings such as `localIndexEnabled` |
| `name` | Yes | Unique instance identifier |
| `url` | Yes | HTTPS ServiceNow URL; loopback HTTP is allowed for local fixtures |
| `username` | Basic/password grant | Username for authentication |
| `authType` | No | `"basic"` or `"oauth"` |
| `grantType` | OAuth only | `"client_credentials"`, `"password"`, or `"authorization_code"` |
| `clientId` | OAuth only | OAuth client identifier |
| `credentialRef` | Basic/client credentials | Canonical local keychain reference |
| `scope` | No | OAuth scope |
| `authorizeUrl` | No | Authorization endpoint for `authorization_code` |
| `tokenUrl` | No | Token endpoint for `authorization_code` |
| `redirectPort` | No | Loopback callback port |
| `callbackPath` | No | Loopback callback path |
| `default` | No | Startup default marker |
| `description` | No | Human-readable description |
Legacy registries may omit `version`; readers treat an omitted value as
version `1`. Any explicit value other than numeric `1` is rejected. CLI writes
always emit numeric `"version": 1`.

The registry file is metadata only and should remain private. Do not add
plaintext credential fields to new configuration.

## Registry path precedence and environment timing

At startup and for CLI reads/writes, resolution is exact:

1. `HAPPY_CONFIG_PATH`, when set. It is the single explicit metadata registry
   path.
2. The user registry (`<homedir>/.config/happy-platform-mcp/instances.json`)
   when it exists.
3. The legacy package-relative `config/servicenow-instances.json` when it
   exists.
4. The singular `SERVICENOW_*` environment fallback only when no registry file
   is available.

Automatic migration is available only when the resolver selects the legacy
package-relative JSON and the default user target is distinct:

```text
readPath:  <package>/config/servicenow-instances.json
writePath: <homedir>/.config/happy-platform-mcp/instances.json
```

In that case `happy-platform-mcp instance migrate` reads the package legacy
source, stores its plaintext credentials in the OS keychain, and writes a
metadata-only version 1 registry to the user target. The source bytes remain
unchanged. `HAPPY_CONFIG_PATH` normally selects both the registry to read and
the target to write, so it must point to a metadata-only version 1 registry.
If it points to a plaintext source (the source and target are the same file),
the CLI refuses before any keychain write and leaves both source bytes and
keychain entries unchanged. Choose a distinct `HAPPY_CONFIG_PATH` target, or
unset it and use the automatic package-legacy -> user-registry workflow.
Never copy credentials into command arguments; for a non-package legacy source,
use a controlled distinct source/target migration workflow or manually
re-register metadata with `instance add`, which prompts for and stores required
credentials before registration.

Migration never reads `SERVICENOW_*` environment variables. Environment-only
credentials must be manually re-registered with `instance add`, whose prompts
collect the required secrets exactly once.

Set `HAPPY_CONFIG_PATH` before launch; do not expect changing it through a
running MCP call to retarget the process.

## Instance Selection

### 1. Per-Call Routing (Recommended)

Add the optional `instance` parameter to any live ServiceNow operation except `SN-Set-Instance`, `SN-Get-Current-Instance`, and `SN-Docs-*`. If omitted, the call uses the current session client's implicit target.

Explicit routes use clients cached by instance name. Use them when overlapping work may target different instances or race with `SN-Set-Instance`; concurrent calls against one stable implicit target do not require explicit routing. Explicit calls to the same instance share that named cached client. `SN-Set-Instance` changes only the current session client's implicit target in memory. It never edits JSON or environment configuration, and a new MCP session or server starts from startup selection again.

Sequential session:

```text
SN-Set-Instance { "instance": "dev" }
SN-Get-Current-Instance {}
SN-List-Records { "table_name": "incident" }
```

Per-call routing:

```text
SN-List-Records { "instance": "prod", "table_name": "incident" }
```

### 2. Startup Selection

For the stdio server, startup selects instances in this order:

1. The named JSON entry in `SERVICENOW_INSTANCE`, when set
2. The entry marked `"default": true`
3. The first configured entry

```bash
SERVICENOW_INSTANCE=prod node src/stdio-server.js
# Selects the configured "prod" entry
```

The published global binary starts the stdio server when invoked without
arguments:

```bash
npm install -g happy-platform-mcp
SERVICENOW_INSTANCE=prod happy-platform-mcp
```

An MCP host may invoke the global binary directly:

```json
{
  "mcpServers": {
    "servicenow": {
      "command": "happy-platform-mcp",
      "env": { "SERVICENOW_INSTANCE": "dev" }
    }
  }
}
```

Or use the package through `npx`:

```json
{
  "mcpServers": {
    "servicenow": {
      "command": "npx",
      "args": ["-y", "happy-platform-mcp"],
      "env": { "SERVICENOW_INSTANCE": "dev" }
    }
  }
}
```

For a source checkout, use `node src/stdio-server.js` for the server and
`node src/cli.js instance list` (or another `instance` command) for CLI
operations. Set `"cwd"` to the checkout directory.

The HTTP server does not use the stdio named override; it starts with the `"default": true` entry, or the first configured entry if none is marked.

The `default` flag only controls startup selection. After startup, calls that omit `instance` use the session client's current implicit target.

### 3. Environment Credential Fallback

If no registry file is available, the legacy singular environment variables
provide one fallback instance named `default`. They are backward-compatible
only and should not be used for new multi-instance setup:

```env
# Set locally; never commit secret values.
SERVICENOW_INSTANCE_URL=
SERVICENOW_USERNAME=
SERVICENOW_PASSWORD=
```

## API Endpoints
```bash
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/instances
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
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/health
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

// Get the configured startup default, or the first entry
const defaultInstance = configManager.getDefaultInstance();

// Get a specific instance
const prodInstance = configManager.getInstance('prod');
// Apply named override, configured default, then first-entry precedence
const selectedInstance = configManager.getInstanceOrDefault(process.env.SERVICENOW_INSTANCE);

// List all instances
const instances = configManager.listInstances();
```

## OAuth Authentication

Each instance can independently use basic auth or OAuth 2.0. Interactive
`instance add` prompts for and stores required Basic, client-credentials, or
password-grant secrets exactly once before registering metadata. Use
`instance test` afterward; do not put secrets in JSON or command arguments.

### Global CLI: dev basic authentication

```bash
happy-platform-mcp instance add
# Select Basic authentication; set the name to dev and enter URL, username, and password when prompted.
happy-platform-mcp instance test dev
```

### Global CLI: prod OAuth client credentials

```bash
happy-platform-mcp instance add
# Select OAuth -> Client credentials; set the name to prod and enter URL, client ID, and client secret when prompted.
happy-platform-mcp instance test prod
```

### OAuth password grant

```bash
happy-platform-mcp instance add
# Select OAuth -> Password grant and enter URL, client ID, username, password, and client secret when prompted.
happy-platform-mcp instance test password-prod
```

Password grant requires both credential references:

```json
"credentialRef": {
  "password": "keychain:instance/password-prod/password",
  "clientSecret": "keychain:instance/password-prod/client-secret"
}
```

### Public authorization code with PKCE

```bash
happy-platform-mcp instance add
# Select OAuth -> Authorization code, choose a public client, and enter:
# URL, client ID, authorize URL, token URL, redirect port, callback path.
happy-platform-mcp instance test public-dev
```

Authorization-code metadata does not require `credentialRef`, a client secret,
or any other static secret. The first test/API call opens the browser
authorization flow; the resulting refresh token is stored in the OS keychain.

For OAuth setup, create the appropriate endpoint under **System OAuth >
Application Registry**, then store only the client identifier and canonical
credential references in the registry. Authorization Code instances use
`authorizeUrl`, `tokenUrl`, `redirectPort`, and `callbackPath` metadata.

`authorizeUrl` and `tokenUrl` must share the instance origin unless the
operator approves the external IdP origin with
`SERVICENOW_OAUTH_TRUSTED_ORIGINS` (comma-separated exact HTTPS origins) in the
environment of both the MCP server and the shell running
`happy-platform-mcp instance …` (the CLI does not read `.env`). The registry and
`SN-Register-Instance` cannot approve an origin. Off-policy endpoints are
rejected at registration and
registry load, and approval is re-checked before every token request; token
requests never follow redirects. See
[Setup Guide](SETUP_GUIDE.md#trusted-external-oauth-identity-providers).

Client Credentials and password-grant tokens are cached in memory and refreshed
before expiry. Authorization Code refresh tokens are stored in the OS keychain
(or the opt-in file store) under an identity key derived from the local OS user,
canonical instance URL, client ID, and authorize/token endpoints — never the
instance name. Renaming an instance keeps its sign-in; a same-named instance on
another URL, client, or IdP never receives it. After upgrading, each
authorization-code instance signs in once; legacy name-keyed tokens are not
read and are deleted after the new sign-in. `instance remove` and `instance
update` changes to `url`, `clientId`, `authorizeUrl`, or `tokenUrl` first delete
the previous identity's refresh token (and legacy name-keyed entry) from both
the OS keychain and the file token store, regardless of which one the server
selected, and print the stores cleaned. If either store reports an error, the
command stops before changing the registry. On Windows the file store is skipped
only when its directory does not exist. Export the same `XDG_CONFIG_HOME` as the
server so the CLI cleans the right file-store directory. Hand edits to the
registry file do not clean tokens. A locked keychain that hides entries cannot
be verified (the keychain binding reports a failed delete as "no entry"; the CLI
re-reads and fails if the entry is still readable), and an unavailable keychain
always stops the command; see the README for the manual workaround. After a
rejected refresh token, browser sign-in starts again; there is no shared
credential or plaintext fallback.

When `SN-Set-Instance` selects a configured instance, the current sequential
session client uses that instance's authentication method. The configuration
itself is unchanged.

## Security and rollback behavior

- Registry files contain metadata only; canonical `credentialRef` values point
  to the OS keychain. There is no plaintext fallback.
- Secret prompts are local and masked. MCP registration rejects
  password/client-secret-shaped fields and never accepts secrets.
- If a credential is missing, `SN-Register-Instance` returns
  `CREDENTIAL_NOT_FOUND` plus the exact local credential-set command(s).
- If the keychain is unavailable, registration and migration abort without a
  registry write.
- Registration uses atomic registry writes. A failed write reports
  `REGISTRY_WRITE_FAILED`; an unsafe compensation reports
  `REGISTRY_ROLLBACK_REQUIRED` and requires manual rollback review.
After docs-only registration, restart the MCP server. Normal live registration
reloads configuration without a restart; restart is required only when reload
fails.

## Troubleshooting

### Instance not found

Check the name with `happy-platform-mcp instance list`, then use
`happy-platform-mcp instance add` or the metadata-only `instance update`
command. Authentication changes require remove/re-add.

### Missing credential

Run the exact `happy-platform-mcp instance credential set <name> --type
password|client-secret` command returned by the MCP registration response, in
an interactive local TTY, then retry.

### Legacy migration required

Run `happy-platform-mcp instance migrate`. The old package-relative file is
left untouched, and unsupported or incomplete entries are not partially
migrated.