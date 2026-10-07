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
  <a href="https://nodejs.org"><img src="https://img.shields.io/badge/node-%3E%3D20-brightgreen.svg?style=flat-square" alt="Node.js Version"></a>
</p>

<p align="center">
  <a href="https://happy-tech.biz">Website</a> |
  <a href="https://github.com/Happy-Technologies-LLC/happy-platform-mcp">GitHub</a> |
  <a href="https://www.npmjs.com/package/happy-platform-mcp">npm</a> |
  <a href="#tool-overview">Tools</a> |
  <a href="CONTRIBUTING.md">Contributing</a> |
  <a href="#support">Support</a>
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

## Support

If you find this project useful, consider supporting its development. Contributions support Happy Technologies LLC.

- [GitHub Sponsors](https://github.com/sponsors/Happy-Technologies-LLC)
- [Buy Me a Coffee](https://buymeacoffee.com/nickzitzer)
## Features

- **Multi-Instance Support** — Connect to multiple ServiceNow&reg; instances simultaneously with per-request routing
- **OAuth 2.0 & Basic Auth** — Per-instance Client Credentials, Resource Owner Password Credentials, and per-user Authorization Code with PKCE
- **Intelligent Schema Discovery** — Automatically discovers table structures and relationships at runtime
- **160+ Tables** — Complete coverage including ITSM, CMDB, Service Catalog, Platform Development, and Flow Designer
- **55 MCP Tools** — Generic CRUD operations that work on any table, plus specialized convenience tools
- **Batch Operations** — 43+ parallel operations tested successfully
- **Local Script Development** — Sync scripts with Git, watch mode for continuous development
- **Natural Language Search** — Query using plain English instead of encoded queries
- **MCP Resources** — 8 read-only resource URIs for quick lookups and documentation
- **Background Script Execution** — Automated server-side script execution via `sys_trigger`
- **Service Catalog AI-Submission** — Browse, inspect, and submit Service Catalog forms programmatically
- **ServiceNow Docs Search** — Optional GitHub-backed docs retrieval and local SQLite FTS search over official ServiceNowDocs markdown

## Quick Start

### Prerequisites

- Node.js 20+
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
# Source checkout: use node src/cli.js <command>, or npm link first.
node src/cli.js instance list
```

### Start the stdio server

After a global install, `happy-platform-mcp` with no arguments starts the
stdio MCP server:

```bash
npm install -g happy-platform-mcp
SERVICENOW_INSTANCE=dev happy-platform-mcp
```

An MCP host can use the global command:

```json
{
  "mcpServers": {
    "happy-mcp-server": {
      "command": "happy-platform-mcp",
      "env": { "SERVICENOW_INSTANCE": "dev" }
    }
  }
}
```

Or use `npx` with `command: "npx"` and `args: ["-y", "happy-platform-mcp"]`.
From a source checkout, use `node src/stdio-server.js`; source CLI commands
use `node src/cli.js instance ...`.

### Configure Instances

**Recommended: register metadata with the local CLI**

The CLI inherits the environment of the process that launches it. The CLI
does not auto-load `.env`; `.env` loading applies to the server/stdio
process only. For CLI use, export `HAPPY_CONFIG_PATH` in the shell (or set it
in the MCP host environment when the host launches the CLI). The actual
default registry path on every OS is
`<homedir>/.config/happy-platform-mcp/instances.json`, using native path
separators for that OS.

Credentials are prompted locally, masked, and stored in the operating system
keychain. A new CLI-written version 1 registry contains instance metadata and
canonical `credentialRef` values only; it does not contain plaintext
credentials. Legacy plaintext registry files are read-only compatibility
inputs, and the singular `SERVICENOW_*` environment fallback is retained only
for backward compatibility.

```bash
happy-platform-mcp instance add
happy-platform-mcp instance list
happy-platform-mcp instance test dev
happy-platform-mcp instance update dev
happy-platform-mcp instance remove dev
happy-platform-mcp instance migrate
```

`instance add` prompts once for the credentials required by the selected auth
mode and stores them before registering metadata. `instance credential set` is
only for rotating an existing credential or completing a metadata-only MCP
registration. `instance update` changes metadata only. Authentication changes
require `instance remove` followed by `instance add`. Secret prompts never put
values in command arguments, logs, or MCP messages. These
prompts require an interactive local TTY. In non-TTY automation, use a
pre-provisioned OS keychain entry and run metadata-only commands; the CLI will
not read secrets from stdin or silently fall back to plaintext.

To select a different metadata registry for the CLI, export
`HAPPY_CONFIG_PATH` before invoking it:

```bash
export HAPPY_CONFIG_PATH="$HOME/.config/happy-platform-mcp/instances.json"
happy-platform-mcp instance list
```

For an MCP server/stdio host, set the same variable in the host environment:

```json
{
  "env": {
    "HAPPY_CONFIG_PATH": "~/.config/happy-platform-mcp/instances.json"
  }
}
```

`HAPPY_CONFIG_PATH` supports `~` and relative paths. For migration, automatic
package-legacy -> user-registry migration is available only when the resolver
selects the package-relative `config/servicenow-instances.json` as
`readPath` and the distinct default user registry as `writePath`.
`HAPPY_CONFIG_PATH` normally selects both `readPath` and `writePath` and must
point to a metadata-only version 1 registry. If it points to a plaintext
source (the same file), the CLI refuses before any keychain write; source
bytes and keychain entries remain unchanged. Choose a distinct
`HAPPY_CONFIG_PATH` target, or unset it for the automatic workflow. Never
copy secrets into command arguments. For a non-package legacy source, use a
controlled distinct source/target workflow or manually use `instance add` and
the masked `instance credential set` prompt.

`SN-Register-Instance` normally applies a live registry reload and does not
require a restart. Restart the MCP server only for docs-only mode or when the
reload fails.

**Legacy compatibility:** `config/servicenow-instances.json` is a read-only
legacy migration input. The singular `SERVICENOW_*` environment variables are
the backward-compatible single-instance fallback only when no registry is
available. The package-relative file is not the CLI's writable location.

**Option B: Single Instance (legacy environment fallback)**

```bash
cp .env.example .env
# Server/stdio only: set legacy SERVICENOW_* variables in .env.
```

### Start the Server

```bash
# Stdio transport (for Claude Desktop); no HTTP token involved
npm run stdio

# HTTP/SSE transport; refuses to start without a valid bearer token
export HAPPY_MCP_API_TOKEN="$(openssl rand -hex 32)"
npm run dev
```

### HTTP Transport Security

The HTTP/SSE transport is built for **one trusted operator**, a single principal. Whoever holds `HAPPY_MCP_API_TOKEN` can use every ServiceNow credential configured on the server, so ServiceNow ACLs on those accounts still decide what they can do. HTTP authentication doesn't replace them. **Shared multi-user hosting isn't supported**: several people or tenants sharing one server or token get no per-user identity, isolation or authorization. Run one server per operator.

- **A token is required on every listener.** HTTP startup fails unless `HAPPY_MCP_API_TOKEN` is 32 random bytes encoded as 64 hex characters (`openssl rand -hex 32`) or 43 base64url characters. Loopback and embedded `createHttpApp` use are no exception. The server checks the encoding but can't measure randomness, so always generate the value. Keep it in a secret store or an untracked `.env`, never commit or log it, and rotate it by restarting with a new value. Stdio doesn't use it.
- **Every route authenticates.** `/health`, `/instances`, `GET /mcp` and `POST /mcp` all require exactly one `Authorization: Bearer <token>` header. The token is compared in constant time. Requests are rejected before body parsing or session setup.
- **Failed attempts are throttled.** After 10 failed attempts from one peer address within a minute, that peer receives `429` with `Retry-After` until the window ends. The throttle is checked before the token, so a blocked peer is refused even with the right token. The budget lives in process memory: it resets on restart, isn't shared between processes, and only tracks a bounded number of peers. Behind a reverse proxy, or Docker port publishing where host clients can share one gateway address, every client shares one budget, so any unauthenticated client that can reach the endpoint can lock the operator out with ten bad requests a minute. Restrict who can reach the proxy and rate-limit there.
- **Host and Origin are checked (DNS-rebinding protection).** `Host` must name the listener (`127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>` on loopback, otherwise the bound address and port) or exactly match a `HAPPY_MCP_ALLOWED_HOSTS` entry. Requests without `Origin`, as native MCP clients send them, are allowed. A present `Origin` must exactly match a `HAPPY_MCP_ALLOWED_ORIGINS` entry, and `null`, wildcard, malformed and duplicate values are rejected. `X-Forwarded-*` and `Forwarded` headers are never trusted.
- **Resource limits.** 16 pending and 32 active SSE sessions (`429` beyond that); `HEAD /mcp` gets `405`. Sessions have a 30-second setup timeout, a 30-minute idle timeout and a 12-hour maximum lifetime. JSON bodies are capped at 8 MiB. Each session processes one POST at a time with up to 8 queued, and at most 8 POSTs are processed at once across sessions. Unanswered JSON-RPC requests are capped at 16 per session and 64 per process. A cancelled request keeps its slot until its handler actually finishes, and its late result is discarded, because tool handlers don't stop early when cancelled. Unknown or closed session IDs are refused.

| Variable | Default | Purpose |
|----------|---------|---------|
| `HAPPY_MCP_API_TOKEN` | none (required for HTTP) | Bearer token: 64 hex or 43 base64url characters |
| `HAPPY_MCP_BIND_HOST` | `127.0.0.1` | Listen address |
| `HAPPY_MCP_ALLOWED_HOSTS` | empty | Comma-separated extra `host[:port]` authorities, such as a reverse-proxy name. No schemes, paths or wildcards |
| `HAPPY_MCP_ALLOWED_ORIGINS` | empty (rejects any `Origin`) | Comma-separated exact browser origins, such as `https://console.example.com` |

#### Reverse proxy

Terminate TLS at the proxy and forward to the loopback listener. If the proxy rewrites `Host` to the upstream address (nginx's default `$proxy_host`, which gives `127.0.0.1:3000`), no extra setting is needed. If it forwards the public name, approve that exact authority:

```nginx
location / {
  proxy_pass http://127.0.0.1:3000;
  proxy_set_header Host $host;   # forwards "mcp.example.com"
  proxy_http_version 1.1;
  proxy_buffering off;           # required for SSE
  proxy_read_timeout 1h;
}
```

```bash
HAPPY_MCP_ALLOWED_HOSTS=mcp.example.com
```

The proxy must pass the `Authorization` header through unchanged. Don't let the proxy add its own credentials in place of the client's token.

#### Migrating existing HTTP clients

This release requires the bearer token for all HTTP use, loopback included.

- **curl and scripts:** add `-H "Authorization: Bearer $HAPPY_MCP_API_TOKEN"` to every request, `/health` included.
- **MCP SSE clients:** configure the client to send the header on both the SSE `GET` and message `POST` requests. With the TypeScript SDK, that's `new SSEClientTransport(url, { requestInit: { headers: { Authorization: 'Bearer ' + token } } })`.
- **Clients that send `Origin`** (for example web views or proxies that forward it): add that exact origin to `HAPPY_MCP_ALLOWED_ORIGINS`. The server sends no CORS headers, so cross-origin browser pages still can't read its responses.
- **Docker:** supply `HAPPY_MCP_API_TOKEN` at run time and set `HAPPY_MCP_ALLOWED_HOSTS` to the authority clients use, for example `localhost:3000,127.0.0.1:3000`. Health checks must authenticate; the image's `HEALTHCHECK` and `docker-compose.yml` already do. See [docs/DOCKER_DEPLOYMENT.md](docs/DOCKER_DEPLOYMENT.md).

### Verify

```bash
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/health
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/instances

# Without the token the server answers 401
curl -i http://localhost:3000/health
```

## Multi-Instance Routing

Every live ServiceNow operation accepts an optional `instance` parameter, except `SN-Set-Instance`, `SN-Get-Current-Instance`, and `SN-Docs-*`. Omitting it uses the current session client's implicit target. Explicit routing is required when overlapping work may target different instances or race with `SN-Set-Instance`; concurrent calls against one stable implicit target do not require it. Explicit calls are cached by instance name, so calls to the same named instance share that client.

At stdio startup, `SERVICENOW_INSTANCE` selects a named JSON entry when set; otherwise startup uses the entry marked `"default": true`, or the first configured entry if none is marked. HTTP sessions use the configured default or first entry. If the JSON file is missing, ServiceNow environment credentials can provide the single fallback instance. The `"default": true` flag is startup configuration. `SN-Set-Instance` changes only the current session client in memory; it never edits configuration, and a new MCP session or server starts from startup selection again.

```javascript
// Uses this session client's current implicit target
await client.callTool({
  name: 'SN-Query-Table',
  arguments: { table_name: 'incident', limit: 10 }
});

// Safely query dev and prod concurrently
await Promise.all([
  client.callTool({
    name: 'SN-Query-Table',
    arguments: { table_name: 'incident', instance: 'dev', limit: 10 }
  }),
  client.callTool({
    name: 'SN-Query-Table',
    arguments: { table_name: 'incident', instance: 'prod', limit: 10 }
  })
]);
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
| **Service Catalog** | 4 | Browse, inspect, and submit catalog forms |
| **ServiceNow Docs** | 5 | Discover, sync, search, and retrieve official ServiceNowDocs markdown |
| **Resources** | 8 | Read-only URIs for table lists, field info |

### Examples

The following transport-neutral examples show an MCP tool name followed by its arguments:

```text
SN-Query-Table
{ "table_name": "incident", "query": "active=true^priority=1", "limit": 10 }

SN-Create-Incident
{ "short_description": "Email service down", "urgency": 1 }

SN-NL-Search
{ "table_name": "incident", "query": "high priority incidents assigned to me" }

SN-Execute-Background-Script
{ "script": "gs.info('Hello');" }

SN-Set-Update-Set
{ "update_set_sys_id": "abc123..." }

SN-Batch-Update
{ "updates": [{ "table": "incident", "sys_id": "id1", "data": { "state": 2 } }] }

SN-Catalog-Search-Items
{ "keyword": "VPN access" }
SN-Catalog-Get-Item
{ "sys_id": "<catalog_item_sys_id>" }
SN-Catalog-Submit
{ "sys_id": "<catalog_item_sys_id>", "variables": { "requested_for": "jsmith", "justification": "Project X" } }

SN-Docs-Families
{}
SN-Docs-Sync
{ "family": "australia" }
SN-Docs-Search
{ "query": "create a Flow Designer action", "family": "australia" }
```

### Local Script Development

Develop scripts locally with version control and automatic sync:

```text
SN-Sync-Script-To-Local
{
  "script_sys_id": "abc123...",
  "local_path": "/scripts/business_rules/validate_incident.js"
}

SN-Watch-Script
{
  "local_path": "/scripts/business_rules/validate_incident.js",
  "script_sys_id": "abc123..."
}
```

### Natural Language Search

```text
SN-NL-Search
{
  "table_name": "incident",
  "query": "active high priority incidents that are unassigned"
}
```

Supports 15+ patterns including field comparisons, text searches, date ranges, logical operators, and ordering.

### ServiceNow Docs Search

Happy MCP can retrieve official ServiceNowDocs markdown directly from GitHub and optionally localize a docs family into a SQLite FTS5 index for fast local search. Local indexing is disabled by default; enable it with `docs.localIndexEnabled=true` in the version 1 registry or `HAPPY_DOCS_ENABLE_LOCAL_INDEX=true`.

```text
SN-Register-Instance
{ "name": "dev", "url": "https://your-instance.service-now.com", "username": "your-username" }

SN-Docs-Families
{}
SN-Docs-Status
{}
SN-Docs-Sync
{ "family": "australia" }
SN-Docs-Search
{ "query": "update set best practices", "family": "australia", "limit": 5 }
SN-Docs-Get
{ "family": "australia", "path": "platform/example.md" }
```

`SN-Register-Instance` accepts metadata only. It never accepts passwords,
client secrets, or other secret fields. If a required local credential is
missing, its response gives the exact `happy-platform-mcp instance credential
set ...` command to run locally. Registration persists metadata, but a
docs-only server must be restarted before live ServiceNow tools are enabled.

SQLite local indexing is optional and disabled by default. Vector search is also optional; enable local indexing, set `HAPPY_DOCS_ENABLE_VECTOR=true`, and use `HAPPY_DOCS_EMBEDDING_PROVIDER=local` to build a sqlite-vec index with deterministic local embeddings. See [ServiceNow Docs Search](docs/SERVICENOW_DOCS_SEARCH.md).

For docs-only deployments without ServiceNow credentials, set `HAPPY_MCP_DOCS_ONLY=true`. If no config file or ServiceNow environment credentials are present, the stdio server falls back to docs-only mode automatically.

### Runtime instance selection

Registration and runtime selection are separate:

- Registration persists an available named instance in the registry.
- `SERVICENOW_INSTANCE` chooses the stdio startup default only.
- `SN-Set-Instance` changes the sequential session's shared implicit target in memory; it does not edit the registry.
- The optional per-call `instance` argument selects an isolated request target, so concurrent calls can route to different instances without racing the sequential target.

## Claude Desktop Integration

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

For a global install, no arguments starts stdio:

```json
{
  "mcpServers": {
    "happy-mcp-server": {
      "command": "happy-platform-mcp",
      "env": { "SERVICENOW_INSTANCE": "dev" }
    }
  }
}
```

For an ephemeral install, use `command: "npx"` with
`args: ["-y", "happy-platform-mcp"]`:

```json
{
  "mcpServers": {
    "happy-mcp-server": {
      "command": "npx",
      "args": ["-y", "happy-platform-mcp"],
      "env": {
        "HAPPY_CONFIG_PATH": "~/.config/happy-platform-mcp/instances.json"
      }
    }
  }
}
```

The registry setting points to metadata; credentials remain in the local OS
keychain. `HAPPY_CONFIG_PATH` is optional with the default user path and must
be set before the server starts. For source installs, use
`node src/stdio-server.js` in the host config and `node src/cli.js instance
<command>` for CLI operations. Restart Claude Desktop after editing its config.

## Authentication

Happy MCP Server supports three credential flows per instance. Interactive
`instance add` captures each required secret once, stores it in the OS
keychain, and registers the metadata. Use `instance credential set` later only
to rotate an existing credential or complete metadata-only MCP registration.
Never place secrets in JSON or command arguments.

### Global CLI: dev basic authentication

```bash
happy-platform-mcp instance add
# Select Basic authentication; set the name to dev, enter URL and username,
# then enter the masked password when prompted. The command stores it once.
happy-platform-mcp instance test dev
```

### Global CLI: prod OAuth client credentials

```bash
happy-platform-mcp instance add
# Select OAuth -> Client credentials; set the name to prod, enter URL and
# client ID, then enter the masked client secret. The command stores it once.
happy-platform-mcp instance test prod
```

### OAuth password grant

```bash
happy-platform-mcp instance add
# Select OAuth -> Password grant; enter URL, client ID, and username, then
# enter both masked prompts. The command stores both credentials once.
happy-platform-mcp instance test password-prod
```

Password grant `credentialRef` must be an object containing both canonical
references:

```json
"credentialRef": {
  "password": "keychain:instance/password-prod/password",
  "clientSecret": "keychain:instance/password-prod/client-secret"
}
```

### Public authorization code with PKCE

```bash
happy-platform-mcp instance add
# Select OAuth -> Authorization code and enter URL, client ID,
# authorize URL, token URL, redirect port, and callback path.
happy-platform-mcp instance test public-dev
```

Public authorization-code metadata requires no `credentialRef` or client
secret. The first test/API call opens the browser flow and stores its refresh
token in the OS keychain.

Refresh tokens are keyed by OAuth identity — local OS user, canonical instance
URL, client ID, and the effective authorize/token endpoints — never by the
instance name, so a renamed instance keeps its sign-in and a same-named
instance on another URL, client, or IdP never receives it. **After upgrading,
sign in once per authorization-code instance:** older name-keyed tokens are not
read or migrated and are deleted after the new sign-in succeeds.
`instance remove`, and `instance update` changes to the URL, client ID, or
authorize/token URL, first delete that identity's refresh token (and any legacy
name-keyed entry) from both the OS keychain and the file token store, whichever
the server used, and name the stores cleaned. If either store reports an error,
the command stops before changing the registry. On Windows the file store is
skipped only when its directory does not exist. Tokens are not cleaned when the
registry file is edited by hand; such a stale token is never used for a
different identity.

The bundled keychain binding (`@napi-rs/keyring` 1.x) reports a locked or denied
delete the same as "no entry", so the CLI re-reads the entry and fails if it is
still readable; a keychain that also hides entries on read cannot be checked —
unlock it before removing. Keychain errors carry no code that separates "no
keychain backend" from "locked", so an unavailable keychain always stops the
command. On Linux without a Secret Service the binding falls back to the kernel
keyring; where neither works (for example containers that block `keyctl`), the
server could only have used the file store: delete the `token-*` files under
`$XDG_CONFIG_HOME/happy-platform-mcp/` (or `~/.config/happy-platform-mcp/`) and
the instance entry from the registry file by hand.

### Trusted external identity providers

Authorize and token endpoints default to the instance itself
(`/oauth_auth.do`, `/oauth_token.do`). A custom endpoint on the instance origin
is always allowed. An external IdP is accepted only when its exact origin is
listed in the environment of every process that loads the configuration — the
MCP server **and** the shell running `happy-platform-mcp instance …` (the CLI
does not read `.env`):

```bash
export SERVICENOW_OAUTH_TRUSTED_ORIGINS=https://login.example.com,https://idp.example.net:8443
```

Entries must be HTTPS origins (loopback HTTP is allowed for local testing) with
no path, query, fragment, credentials, or wildcard; malformed entries fail
closed. The allow-list cannot be set through the instance registry or MCP tools.
It is checked when instances are registered or loaded and again immediately
before every token request, and token requests never follow redirects. A
registry that already uses an external IdP fails to load — in the server and in
every CLI command, including `instance remove` — until its origin is added. If
the server uses the file token store with a custom `XDG_CONFIG_HOME`, export the
same `XDG_CONFIG_HOME` for the CLI so removal cleans the right directory.

### Optional plaintext refresh-token storage (POSIX only)

The OS keychain remains the default (unset or `SERVICENOW_TOKEN_STORE=keychain`).
Explicitly opt in with `SERVICENOW_TOKEN_STORE=file` in the server process environment
to persist authorization-code refresh tokens as plaintext `token-<sha256-hex>` files
(named by the SHA-256 of the account key, so case-insensitive filesystems cannot alias
accounts that differ only by case) in `$XDG_CONFIG_HOME/happy-platform-mcp`, or
`~/.config/happy-platform-mcp` when XDG is unset. Earlier `token-<account>` files are
never read; authorize again. `XDG_CONFIG_HOME` must be an absolute trusted path. This
does not change the keychain storage of instance passwords or OAuth client secrets.

The file store checks current-user ownership, a private 0700 directory and regular
0600 token files; unsafe existing directories/files, symlinks, writable untrusted
ancestors and setup/permission errors fail closed. Every existing ancestor (after
resolving symlinks) is checked before any directory is created and must be owned by
the current user or root and not group/world-writable unless sticky; errors name the
offending path and required ownership/mode. If the directory already holds
configuration with broader permissions, inspect it and make it private before opting
in; the server does not silently repair exposed existing storage. Windows file mode
bits do not enforce Windows ACL privacy, so file storage is rejected there: keep
the OS-keychain default. See [setup details](docs/SETUP_GUIDE.md#refresh-token-storage).

Any process running as the same OS user can read these plaintext tokens. Exclude the
directory from shared/synced backups, or protect backups as secrets (including access
controls and encryption). Do not commit, log or upload its contents. Switching stores
does not migrate tokens; authorize again in the selected store. Each write uses a
unique exclusive 0600 temporary file and same-directory atomic rename, so concurrent
writers do not share temporary files and readers see complete tokens. This does not
serialize OAuth refresh exchanges, solve cross-process refresh-token rotation, or
guarantee power-loss durability; use one refreshing process per identity.

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
# Test configured credentials without exposing them in shell history.
happy-platform-mcp instance test <instance-name>

# Check server health (HTTP transport)
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/health
```

### Common Problems

- **Multi-instance not working:** Verify the version 1 registry is valid JSON and has one `"default": true` instance when a startup default is needed. Normal live registration reloads without a restart; restart only for docs-only mode or a reload failure.
- **Tools not appearing:** Check MCP Inspector connection and server logs.
- **Auth failures:** Run `happy-platform-mcp instance test <instance-name>` and verify the local keychain credential reference and required ServiceNow roles.
- **HTTP `401`/`403`/`429`:** `401` means a missing, duplicate or wrong bearer token. `403` means the `Host` or `Origin` header isn't approved (see [HTTP Transport Security](#http-transport-security)). `429` means too many failed attempts from your address or too many open sessions; wait for `Retry-After`.
- **SSE disconnects in Docker:** Enable keepalive (default 15s). See `docs/SSE_DOCKER_SETUP.md`.

### Debug Mode

```bash
DEBUG=true npm run dev
```

## Known Limitations

- Flow Designer logic blocks cannot be created via REST API (use the UI)
- Flow compilation/validation must be done in the UI
- UI Policy Actions linking requires a background script workaround

See the API reference and tool-specific guides under `docs/` for details.

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
