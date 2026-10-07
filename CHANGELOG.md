# Changelog

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

### Security

- Pin MCP SDK 1.32.1 and Axios 1.20.0, refresh affected transitive dependencies, and retain only development-tool overrides. The bundled SDK now supports Hono 2 natively, without a prepack manifest rewrite.
- Restrict npm releases to an explicit runtime, configuration-example, documentation, asset, and consumer-verification allow-list. Docker installs production dependencies from the committed lockfile instead of re-resolving versions.
- Confine generated fix-script files to new direct children of `./scripts`, created with mode 0600 on POSIX (VULN-008). `SN-Create-Fix-Script` now rejects a `script_name` that is not 1-100 letters, digits, `.`, `_` or `-` starting with a letter or digit, or that is a Windows reserved device name (`CON`, `NUL`, `COM1`, `LPT1`, …). Every fix-script writer refuses a symlinked or non-directory `scripts` path and never follows or overwrites an existing target. **Breaking:** names containing spaces, path separators or other characters, and device names, must be renamed.
- Serialize update-set names and sys_ids as JavaScript string literals in the `SN-Set-Update-Set` `sys_trigger` and manual fix-script fallbacks, and keep caller/instance-supplied descriptions and names out of generated block comments; they appear only as data literals (`updateSetName`, `fixScriptMetadata`) (VULN-009, VULN-010).

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
