# Changelog

## Unreleased

### Security

- Windows browser sign-in launches the default browser through the shell URL handler (`rundll32.exe url.dll,FileProtocolHandler <url>`, no shell, URL as one argument); `explorer.exe <url>` silently opened nothing on native Windows. The opener now only hands `http(s)` URLs to any OS handler. A `windows-latest` CI job runs the real sign-in through the system browser against a loopback fake IdP (VULN-004).
- ServiceNow docs tools (VULN-013, -014, -015, -016, -017) only fetch from `raw.githubusercontent.com/ServiceNow/ServiceNowDocs`, reject redirects, and only accept families and branches listed in its `main/llms.txt`. Document paths and sync links are validated before any cache lookup or request. Responses are streamed under size, line, aggregate and time limits, HTTP errors no longer echo response bodies, and cached documents are tied to the branch they came from.
- **Breaking:** docs requests no longer send `GITHUB_TOKEN` or `docs.githubToken`. Remove these settings; authenticated docs retrieval is not supported. Existing local docs caches are discarded on first open and must be re-synced.

## 5.2.0 - 2026-08-28

### Added

- `SN-Execute-Background-Script` now captures and returns script output: `gs.info`/`gs.print` log lines are recovered from a bounded `syslog` window and returned as `logs`, the thrown error is surfaced on failure (previously swallowed by a bare `try`/`finally` with no `catch`), and three distinguishable outcomes (`completed`, `failed`, `timeout`) replace the old single success-looking response. A `wait: false` opt-out preserves fire-and-forget behavior.
- Per-request `progressToken` isolation via `AsyncLocalStorage` so concurrent tool calls (the project's own guidance recommends 5-10 at once) no longer collide on a shared client instance's token or progress counter.
- Process-level `unhandledRejection`/`uncaughtException` guards in all three entrypoints (`src/server.js`, `src/http-server.js`, `src/stdio-server.js`) via a shared `src/process-guards.js` — a single dropped notification can no longer terminate every concurrent session.

### Security

- Removed tracked ServiceNow credential material from `start-mcp.sh` and deleted the committed `.env.backup`; the startup script now fails closed when required configuration is absent.
- Resolved npm audit failures (brace-expansion, fast-uri, ip-address, js-yaml, hono) that were blocking the `security-audit` CI gate on every PR.

### Fixed

- `notifications/progress` payloads now conform to the MCP spec (`progressToken` required, `progress` numeric, text in `message`); spec-compliant clients like Cursor no longer reject them and drop the connection (#58).
- `server.notification()` rejections are now handled instead of becoming `unhandledRejection` process crashes (#50).
- `sys_trigger` `next_action` is now formatted as UTC instead of local time — scripts were scheduling ~2 hours out (one UTC offset) instead of ~1 second (#52).
- `getRecords()` now forwards `sysparm_offset` and `sysparm_order_by` to the Table API; pagination and sorting were silently ignored for all nine affected tool handlers (#55).
- Removed the false `execution_method` enum from `SN-Execute-Background-Script` — the handler never read it and the advertised `ui` path was dead code that always threw.

## Unreleased

### Breaking

- The HTTP/SSE transport now requires `HAPPY_MCP_API_TOKEN` for every listener, loopback and embedded `createHttpApp` use included. The token must be 32 random bytes encoded as 64 hex characters (`openssl rand -hex 32`) or 43 base64url characters; startup or app creation fails when it's missing or malformed. Stdio is unchanged.
- `/health`, `/instances`, `GET /mcp` and `POST /mcp` all require `Authorization: Bearer <token>`. Update curl scripts, monitoring probes, Docker and Kubernetes health checks, and MCP SSE clients (both the event stream and message POSTs). The Docker image now listens on `0.0.0.0` inside the container and authenticates its `HEALTHCHECK`. `docker-compose.yml` requires the token, publishes on `127.0.0.1:3000` and authenticates its health check.
- `Host` must name the listener or match `HAPPY_MCP_ALLOWED_HOSTS`, and a present `Origin` must exactly match `HAPPY_MCP_ALLOWED_ORIGINS`. Containers and reverse proxies must list the authority clients use, for example `HAPPY_MCP_ALLOWED_HOSTS=localhost:3000,127.0.0.1:3000`.
- `validateHttpTransportSecurity` is removed from `src/http-server.js`. Use `loadHttpSecurityConfig(env)` and pass `apiToken`, `allowedHosts` and `allowedOrigins` to `createHttpApp`.
- The supported HTTP deployment is one trusted operator. Shared multi-user hosting behind one server or token is unsupported; ServiceNow ACLs remain the authority for what the configured accounts can do.

### Security

- Compare the bearer token in constant time and reject missing, duplicate or malformed `Authorization` headers before routing, body parsing or session setup. After 10 failed attempts from one peer within a minute, that peer receives `429` with `Retry-After`, even with the right token. The budget is in-process with bounded state, so it resets on restart and isn't shared between processes. Behind a proxy or Docker port publishing, clients can share one address and therefore one budget, so restrict access and rate-limit at the proxy.
- Validate `Host` and `Origin` before parsing or allocating anything (DNS-rebinding protection). `null`, wildcard, malformed and duplicate values are rejected, and `X-Forwarded-*`/`Forwarded` headers are never trusted.
- Bound the SSE lifecycle: 16 pending and 32 active sessions; 30-second setup, 30-minute idle and 12-hour lifetime limits; 8 MiB JSON bodies; one POST at a time per session with up to 8 queued; 8 concurrent POSTs per process; and 16 unanswered JSON-RPC requests per session and 64 per process. A cancelled request keeps its slot until its handler finishes, and its late result is discarded. `HEAD /mcp` is answered with `405` instead of opening a session. Cleanup is registered before setup awaits, so a client that disconnects mid-setup can't leave a ghost session. Unknown or closed session IDs are refused before the SDK sees them, and `src/server.js` closes every session on `SIGTERM`/`SIGINT`.
- Pin MCP SDK 1.32.1 and Axios 1.20.0, refresh affected transitive dependencies, and retain only development-tool overrides. The bundled SDK now supports Hono 2 natively, without a prepack manifest rewrite.
- Restrict npm releases to an explicit runtime, configuration-example, documentation, asset, and consumer-verification allow-list. Docker installs production dependencies from the committed lockfile instead of re-resolving versions.
- Bind persisted authorization-code refresh tokens to the OAuth identity they were issued for: a versioned `identity-v1-<sha256>` key over the local OS user, canonical instance URL, OAuth client ID, and effective authorize/token endpoints, instead of the mutable instance name. Same-named or renamed instances can no longer retrieve or send another identity's refresh token. **Upgrade note:** legacy name-keyed entries are never read or migrated; each authorization-code instance prompts for browser sign-in once, after which the legacy entry is deleted. `instance remove` and identity-changing `instance update` (URL, client ID, authorize/token URL) clear the affected identity and legacy entries from both the OS keychain and the file token store (whichever the server selected; the file store is skipped on Windows only when its directory is absent), print the stores cleaned, and stop before changing the registry if either store reports an error. Hand edits to the registry file do not clean tokens.
- OAuth authorize/token endpoints must share the instance origin unless the operator lists the external IdP origin in `SERVICENOW_OAUTH_TRUSTED_ORIGINS` (comma-separated exact origins). The policy is enforced on registration (CLI and `SN-Register-Instance`), registry/environment load, and immediately before every token request; token requests never follow redirects. Registries that already point at an external IdP fail to load until its origin is approved — export the variable for both the server and the `happy-platform-mcp instance` CLI, which does not read `.env` (likewise any custom `XDG_CONFIG_HOME` used by the file token store).
- Token-request errors redact the authorization `code` and PKCE `code_verifier` from retained request bodies.
- Upgrade `@napi-rs/keyring` to 2.1: locked, denied or inaccessible keychains now raise errors instead of reporting a failed delete as "no entry", so `instance remove`/`update` and stored-credential deletion fail closed rather than reporting success with the token or secret retained. An unavailable keychain still stops `instance remove` (backend absence cannot be told apart from a locked store); the README documents the manual workaround.
- `SN-Set-Current-Application` and its UI-session client return and log only a redacted message, error code, and HTTP status; the raw Axios error (`original_error`, request config, headers) is no longer retained.
- Instance, OAuth endpoint, callback-path and trusted-origin URLs are capped at 4096 characters at registry load, environment fallback, `SN-Register-Instance`, the OAuth endpoint policy and direct `ServiceNowClient` construction; trailing-slash normalization uses linear scans instead of backtracking regexes, so long slash runs are rejected or normalized promptly while the instance origin and path prefix still bound credentialed requests.
- Bulk inputs are validated as a whole before the first ServiceNow write; update-set lookups use bounded `sys_idIN` chunks of at most 100 IDs and paginate instead of silently truncating at 1000/5000 records; batch `${name}` references resolve in one linear pass per operation. `SN-Natural-Language-Search` advertises a 2048-character `query` and logs only its length.
- Confine generated fix-script files to new direct children of `./scripts`, created with mode 0600 on POSIX (VULN-008). `SN-Create-Fix-Script` now rejects a `script_name` that is not 1-100 letters, digits, `.`, `_` or `-` starting with a letter or digit, or that is a Windows reserved device name (`CON`, `NUL`, `COM1`, `LPT1`, …). Every fix-script writer refuses a symlinked or non-directory `scripts` path and never follows or overwrites an existing target. **Breaking:** names containing spaces, path separators or other characters, and device names, must be renamed.
- Serialize update-set names and sys_ids as JavaScript string literals in the `SN-Set-Update-Set` `sys_trigger` and manual fix-script fallbacks, and keep caller/instance-supplied descriptions and names out of generated block comments; they appear only as data literals (`updateSetName`, `fixScriptMetadata`) (VULN-009, VULN-010).
- CI scans the full git history and the exact packed npm tarball (including bundled dependencies) with a pinned, checksum-verified gitleaks before merge and before npm publish. A `servicenow-env-credential` rule detects literal `SERVICENOW_PASSWORD`/`SERVICENOW_CLIENT_SECRET` values; only owner-confirmed rotated historical findings from #67 are baselined by exact fingerprint, so any new occurrence fails.
- `SN-Natural-Language-Search` parsing is now bounded. `parseNaturalLanguage()` rejects non-string queries (`TypeError`) and queries over 2048 characters (`RangeError`, exported as `MAX_NATURAL_LANGUAGE_QUERY_LENGTH`) before any parsing. The overlapping backtracking regexes, which took seconds for 2048-character inputs and grew cubically with length, are replaced by a single-pass tokenizer with linear-time pattern matching. Captured names, search terms and dates can no longer contain `^`, line breaks or other control characters, so a value cannot inject extra encoded-query conditions or `ORDERBY` clauses.
- Intentional natural-language parsing changes that come with the bounded parser: state words now produce valid conditions (`open` → `state=1^ORstate=2^ORstate=3`, `closed` → `state=7`, previously bare `1^OR…`/`7`); captured values end only at whole connector words (`assigned to John Andrews` is no longer cut at `And`); a value cannot be empty or start with `and`/`or`; matched text is removed where it matched rather than at its first occurrence; and `matchedPatterns[].pattern` is a stable pattern id (for example `priority-code`) instead of the regex source. Raw encoded queries are still passed through unchanged when no natural-language pattern matches.

### Breaking

- **Input limits.** `SN-Batch-Create`/`SN-Batch-Update`: at most 100 operations, `data` an object of at most 64 KiB as JSON, valid table names, `save_as` matching `^[A-Za-z_][A-Za-z0-9_]{0,63}$`, update `sys_id` 32 lowercase hex characters. `SN-Create-Workflow`: at most 100 activities and 200 transitions, names ≤255 characters, script/condition fields ≤256 KiB, whole input ≤4 MiB, and a valid `table`. `SN-Move-Records-To-Update-Set`: target and record IDs must be 32-character lowercase hex sys_ids, at most 200 record IDs, `time_range` in `YYYY-MM-DD HH:MM:SS`, and `source_update_set` without `^`/control characters; time-range moves and `SN-Clone-Update-Set` fail before any write when more than 10,000 records match. `SN-Clone-Update-Set` fetches all source records before creating the clone. Batch references now replace every occurrence of a saved `${name}` (previously only the first), and still leave unknown or later names literal. `ServiceNowClient` rejects non-`http(s)` instance URLs and URLs with credentials, query or fragment, and trims all trailing slashes.
- **Batch and workflow references.** `SN-Batch-Create` rejects duplicate `save_as` names and the reserved names `__proto__`, `constructor`, `prototype` and `operation_N`; returned `sys_ids`/`activity_sys_ids` are null-prototype objects. `SN-Create-Workflow` transition `from`/`to` and `start_activity` strings must name a declared activity `id`, a unique activity `name`, or an existing activity sys_id (validated before any write); names are now resolved to the created activity sys_ids, and numeric indexes resolve correctly even when activities declare an `id`.

### Changed

- Replace nodemon with the built-in Node.js watcher for `npm run dev`.
- Native SQLite indexing requires dependency install scripts. Use the supported npm 10/11 CI install policy; npm 12 blocks unapproved native install scripts by default and needs explicit operator approval before SQLite can load.

## 5.1.0 - 2026-07-27

### Added

- User-owned multi-instance registration with separate dev, test, and production profiles, `HAPPY_CONFIG_PATH` overrides, atomic registry writes, and migration from legacy package-local configuration.
- Interactive `happy-platform-mcp instance` commands for adding, listing, updating, testing, removing, migrating, and securely provisioning credentials.
- Basic, OAuth password, OAuth client-credentials, and public authorization-code registrations backed by operating-system keychain storage rather than registry JSON.
- Metadata-only `SN-Register-Instance` MCP setup support and immediate per-call routing to newly registered instances.

### Security

- Redacted credentials, authorization headers, newly issued OAuth tokens, keychain failures, and registration rollback diagnostics from errors and logs.
- Added strict authentication schemas, credential identity checks, serialized mutations, state-aware rollback, and fail-closed migration source validation.
- Treat empty or whitespace-only keychain values as missing credentials and prevent incomplete instance metadata from being persisted.
- Refreshed and constrained the development test dependency graph to patched globbing packages so release audits remain clean.

### Fixed

- Await asynchronous keychain operations before continuing registration or OAuth flows.
- Preserve canonical missing-path component order while retaining same-file and symlink-alias migration refusal.

## 5.0.0 - 2026-07-23

### Breaking changes

- Raised the minimum supported Node.js runtime from 18 to 20 for the patched HTTP adapter dependency.

### Added

- Optional per-call `instance` routing for every live ServiceNow operation except `SN-Set-Instance`, `SN-Get-Current-Instance`, and `SN-Docs-*`. Named routes use clients cached by instance name, preventing cross-instance and session-switch races without changing the session client's implicit target.

### Security

- Remediated npm advisories by pinning MCP SDK 1.29.0 and Axios 1.18.1, scoping the SDK's HTTP adapter to patched version 2.0.11, and refreshing affected transitive dependencies. `npm audit` now reports zero vulnerabilities.

## 4.0.0 - 2026-07-16

### Breaking changes

- HTTP/SSE deployments on a non-loopback `HAPPY_MCP_BIND_HOST` must set `HAPPY_MCP_API_TOKEN`; clients must send it in `Authorization: Bearer <token>`.

### Added

- Per-user OAuth `authorization_code` authentication with PKCE and a loopback callback. Refresh tokens are stored in the operating system keychain under the current OS user and instance name.

### Contributors

- Thanks to [@cbonitz8](https://github.com/cbonitz8) for the authorization-code OAuth implementation in PR #43.

## 3.3.0 - 2026-07-16

### Security

- HTTP transport now listens on loopback by default. A non-loopback `HAPPY_MCP_BIND_HOST` requires `HAPPY_MCP_API_TOKEN`.
- Each SSE connection now receives its own ServiceNow client, preventing instance and credential state from crossing sessions.

### Fixed

- ServiceNow REST failures now include the response body's message and detail.

### Dependencies

- Updated `express` to `5.2.1`, `form-data` to `4.0.6`, and `hono` to `4.12.30`.

### Contributors

- Thanks to [@OlmsteadNick](https://github.com/OlmsteadNick) for diagnosing and proposing the REST error detail improvement in #39 and PR #42.
- Thanks to [@dependabot](https://github.com/dependabot) for the dependency update alerts in PR #37 and PR #38.

## 3.2.3 - 2026-05-14

- Added env-only OAuth `client_credentials` support through `SERVICENOW_OAUTH_GRANT_TYPE`.
- Updated transitive dependencies: `hono` to `4.12.18` and `fast-uri` to `3.1.2`.
- Thanks to [@davidkarlsen](https://github.com/davidkarlsen) for contributing the OAuth grant-type support in PR #32.
