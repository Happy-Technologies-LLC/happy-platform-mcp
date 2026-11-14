# Migrating to happy-platform-mcp 7.0

7.0 is a Rust rewrite of the server. It keeps all 60 tools, their names, their arguments (one optional argument is added, section 6) and their normal output, and it keeps your instance registry and saved credentials. This page says how to get 7.0, what you must do once, and what looks different.

7.0.2 is released on the npm `next` channel first. The `latest` tag, and the Docker `:latest` tag, stay on 6.x until 7.0 is promoted in a later, separate step. Nothing changes for you until you choose 7.0.

It is written for people who run happy-platform-mcp on their own machine or server and keep its credentials in the OS keychain. For install commands see the README ("Install 7.0 (preview)"); to check a download see [VERIFYING_RELEASES.md](VERIFYING_RELEASES.md).

## Install and upgrade

### A. Which channel you are on

| You run | What you get | To try 7.0 | To stay on 6.x |
|---|---|---|---|
| `npx -y happy-platform-mcp` (no tag) | 6.2.0 until 7.0 is promoted, then 7.x | `npx -y happy-platform-mcp@next` | pin `@6` |
| `npx -y happy-platform-mcp@6` | the JavaScript 6.x server; a 7.x release never matches `@6` | n/a | nothing to do |
| `npm install -g happy-platform-mcp` | 6.2.0 until promotion | `npm install -g happy-platform-mcp@next` | `npm install -g happy-platform-mcp@6` |
| `docker run nczitzer/happy-platform-mcp:latest` | 6.x until promotion | `:next`, `:7.0.2`, or pin the digest | keep `:latest`, or pin a 6.x tag |
| Homebrew | new in 7.0 | `brew install happy-technologies-llc/tap/happy-platform-mcp` | n/a |
| Release archive | new in 7.0 | download from the GitHub Release `v7.0.2` | n/a |

### B. npm

The npm package is a small Node launcher that starts the Rust binary for your platform. It pulls in one of five platform packages (`@happy-technologies/happy-platform-mcp-darwin-arm64`, `-darwin-x64`, `-linux-x64-gnu`, `-linux-arm64-gnu`, `-win32-x64`) and provides all three command names: `happy-platform-mcp`, `servicenow-mcp-server` and `happy-platform-mcp-http`. Node is needed only to run that launcher; the Homebrew, archive and image installs do not need it.

`happy-platform-mcp@6` is the JavaScript server, not a 7.0 install, and still needs Node 20 or newer.

### C. Docker

- `docker run` keeps using an image it already cached; it does not pull a newer one on its own. Run `docker pull nczitzer/happy-platform-mcp:<tag>` when you want a newer image.
- Before promotion the 7.0 image has only the tags `7.0.2` and `next`, plus a digest. It does not get `latest`, `7` or `7.0` until promotion, so an existing launcher that runs `:latest` keeps running 6.x.
- To test 7.0 use `:next`, or pin the digest: `nczitzer/happy-platform-mcp@sha256:<digest>`. The digest is in the GitHub Release notes. Pinning the digest is the safer choice, because `next` moves with each 7.x preview.
- The image is built on a glibc base (`gcr.io/distroless/cc-debian12:nonroot`) and has no shell. A container has no OS keychain session, so pass credentials as environment variables (`SERVICENOW_INSTANCE_URL`, `SERVICENOW_USERNAME`, `SERVICENOW_PASSWORD`), as with the 6.x image. Section 12 lists the keys a `.env` file still loads.
- The HTTP server still requires `HAPPY_MCP_API_TOKEN`, as since 6.0.
- The default command is still the HTTP server on port 3000, as in 6.x. For stdio (Claude Desktop and other MCP clients), run `docker run -i --rm nczitzer/happy-platform-mcp:next servicenow-mcp-server`.
- The container runs as the distroless `nonroot` user (uid 65532), not root. Mounted files, such as `/app/config/servicenow-instances.json`, must be readable by uid 65532. Generated fix scripts are written to `/app/scripts`, which uid 65532 owns; mount a host directory there that uid 65532 can write to if you want to keep them.
- The healthcheck in the repository's `docker-compose.yml` runs `node`. The 7.0 image has no Node.js or shell, so that healthcheck fails with the 7.0 image. Remove it, or replace it with an external check, when you run 7.0 with compose.

### D. macOS: a keychain prompt may appear

7.0 reads the credentials that 6.x saved, under the same keychain service name (`happy-platform-mcp`) and account names. The 7.0 preview binaries are not signed or notarised yet. Because of that, macOS may ask you to allow the new binary to read items that 6.x created, and may ask again after each upgrade, because the binary changes. Approve the prompt for your own `happy-platform-mcp` binary. Developer ID signing and notarisation are added before 7.0 is promoted to `latest`.

### E. Linux

- glibc only: `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`. There is no musl or Alpine build. The npm launcher on a musl system prints a clear error and exits with 127.
- The keychain is the Secret Service over D-Bus (GNOME Keyring, KWallet and similar). The binary needs the system library `libdbus-1` at run time (package `libdbus-1-3` on Debian and Ubuntu) and glibc 2.34 or newer. The Docker image already contains it.
- A headless server or container with no running Secret Service answers `KEYCHAIN_UNAVAILABLE` (section 9) and never falls back to a file. Set the credentials through the environment there.
- Difference from 6.x: the 6.x README says that without a Secret Service the JavaScript binding falls back to the Linux kernel keyring. 7.0 has no such fallback.

### F. Windows

Credentials are kept in Windows Credential Manager. The preview binary is not signed yet, so Windows may warn when you first run it.

### G. Homebrew and release archives

- `brew install happy-technologies-llc/tap/happy-platform-mcp`
- Archives: `happy-platform-mcp-<target>.tar.xz` (`.zip` on Windows) with a `.sha256` file each, for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`. Each archive holds all three binaries. They are attached to the GitHub Release `v7.0.2`, which is marked as a prerelease until promotion.
- Each release also carries `SHA256SUMS`, its Sigstore bundle `SHA256SUMS.sigstore.json` and a CycloneDX SBOM (`happy-platform-mcp.cdx.json`); the archives have build provenance. See [VERIFYING_RELEASES.md](VERIFYING_RELEASES.md).

### H. Same files, same credentials

7.0 uses the same instance registry as 6.x: the file named by `HAPPY_CONFIG_PATH`, else `~/.config/happy-platform-mcp/instances.json`, else the legacy `config/servicenow-instances.json` (read only). Your instances carry over. Saved secrets carry over too, but need the one-time bind in section 1.

## What you must do

### 1. Bind your saved credentials once (only if you saved them with 6.x)

7.0 ties every saved password or client secret to the instance it was saved for. The secret is sent to that instance's address and nowhere else.

Secrets saved by 6.x have no tie yet. 7.0 will not send them. Run this once for each instance, in a terminal:

```
happy-platform-mcp instance credential bind <name>
```

What the command does:

- It needs an interactive terminal. It refuses to run from a script or a pipe.
- It asks `Re-enter the saved <password|client secret> for '<name>' to bind it:` (input hidden). It binds only if what you type matches the saved secret, so a program that cannot see the secret cannot bind it to an address of its choosing. A mismatch stops with `CREDENTIAL_SECRET_MISMATCH` and nothing is bound.
- It asks `Bind the saved credentials for '<name>' to <address>? They will only ever be sent there.` The address comes from the instance's current registry entry. Check it before you confirm.
- The default answer is No.
- It stops with an error if the secret is already bound to a different address. In that case use `instance credential set <name>` (see section 3).
- Secrets you save with 7.0 (`instance add`, `instance credential set`, `instance migrate`) are bound at that moment, so you only need `bind` for secrets that 6.x saved.
- Every check of the re-entered secret waits a fixed 1 second before it answers, whether it matches or not. This stops the command from being used as a fast guess oracle. Do not wrap it in a tight loop.
- Instances that use environment variables (`SERVICENOW_INSTANCE_URL`, `SERVICENOW_USERNAME`, `SERVICENOW_PASSWORD`) are not affected. Nothing is read from the keychain.

`instance credential bind` is listed in `instance --help`: 7.0 adds one line right after the 6.2.0 usage text, `happy-platform-mcp instance credential bind <name>   (7.0: bind a secret saved by 6.x to the registered URL)`. The rest of the usage text is kept word for word as in 6.2.0, so scripts that compare it still work if they ignore that one line. The error messages below name the command.

### 2. Two new credential errors

If a call fails with one of these, nothing was sent to ServiceNow.

| Error code | What it means | What to do |
|---|---|---|
| `CREDENTIAL_ORIGIN_UNBOUND` | The saved credential for this instance has no address binding. This is normal for secrets saved by 6.x. | Run `happy-platform-mcp instance credential bind <name>`. |
| `CREDENTIAL_ORIGIN_MISMATCH` | The credential was saved for one address, but the instance now points at another. Someone changed the URL in the registry file, or an agent changed it. | First check the instance URL. If the new address is not your instance, put the old URL back. Only if the instance really moved, run `happy-platform-mcp instance credential set <name>` and confirm the new address. |

Only the address (scheme, host and port) is compared. The path is not.

The `SN-Register-Instance` tool follows the same rule. An agent can register a name only if a credential for that name is already stored and bound to the same address. If it is bound to a different address, the tool returns `CREDENTIAL_ORIGIN_MISMATCH` and lists the command to run in your terminal.

### 3. `instance credential set` now shows the target and asks you to confirm

Before it asks for the password, `instance credential set <name>` prints:

```
Saving the credential for <address>; it will only ever be sent there.
```

If the secret is not yet bound to that address, or is bound to a different one, you get a confirm question and the default is No:

- no binding yet: `Bind the credential for '<name>' to <address>?`
- bound elsewhere: `The saved credential for '<name>' is bound to <old address>. Check the instance URL: re-bind it to <new address>?`

Answer No and nothing changes. This is on purpose: it stops anyone who edits the registry file from getting you to type a password for the wrong host.

`instance add` and `instance migrate` also bind at save time.

## What looks different in tool output

### 4. Tool annotations

`tools/list` now carries MCP annotations (such as `readOnlyHint`) on all 60 tools. 29 tools are marked read-only. Names are the same as 6.2.0. One input schema gains an optional argument: `allow_trigger_fallback` on SN-Execute-Background-Script (section 6). Two descriptions change: SN-Batch-Create (section 5) and SN-Execute-Background-Script (section 6).

The hints are advice for the client, for example to skip a confirmation for a read. The server never uses them to decide what is allowed. Older clients ignore them.

### 5. SN-Batch-Create: the `transaction` wording

The 6.2.0 description called `transaction` "All-or-nothing". It never was. 6.2.0 stopped at the first failed operation and did not roll anything back. 7.0 says so:

> Stop at the first failed operation (default: true). Not all-or-nothing: a mid-batch failure can leave records created by earlier operations in place; the error reports them.

The behaviour is the same. The error text is longer. It keeps the 6.2.0 sentence and adds the records that stay created:

```
Error: Batch create failed at operation 2: <reason>
Already created and not rolled back: {"first": "<sys_id>", ...}. Do not re-run the whole batch.
```

Do not re-run the whole batch after this error: it would create the earlier records a second time.

### 6. "Outcome unknown" for 408 and 5xx on writes

When a tool changes data and ServiceNow answers 408 (timeout) or any 5xx, or the connection fails after the request left, 7.0 does not say "failed". The change may have been applied. The tool returns an error with this fixed text:

> The request reached ServiceNow but its result could not be confirmed. Do not retry; check the affected records first.

It applies to every tool that writes or runs code, including SN-Run-ATF-Test and SN-Run-ATF-Suite. Only a definite refusal (a 4xx answer other than 408) is reported as a plain failure. If a call fails before anything is sent (a bad argument, a missing credential), you get the normal error and nothing was changed. If part of a multi-step change was applied and part was not, you get a fixed "Some changes were committed ... Do not re-run the whole call" message.

For automation: do not retry a write on this message without checking the record first.

SN-Execute-Background-Script backup route: the tool normally runs scripts through Scripts - Background (`/sys.scripts.do`) and never retries. Only when that route certainly did not run the script does it fall back to a scheduled job (`sys_trigger`). In 6.2.0, if creating that job failed, the tool always wrote a fix script and told you to run it by hand, even when the job might already exist and run seconds later. In 7.0 the fix script is written only when the job certainly was not created. When the outcome is unknown, the error names the scheduled job and tells you to check it and the System Log, and not to re-run the script.

**The scheduled-job fallback is opt-in.** That fallback runs the script as a scheduled job in the scheduler's session, not yours: no rollback context is recorded, and tracked configuration writes can land in the Default update set. In 6.2.0 the tool used it on its own whenever `/sys.scripts.do` refused the script. 7.0 does not. If the processor refuses, the call stops, says the script was not run, and names the two ways to allow the fallback:

- pass `allow_trigger_fallback: true` on the call (a new optional argument), or
- set `HAPPY_MCP_ALLOW_TRIGGER_FALLBACK=1` in the server environment.

An explicit `allow_trigger_fallback: false` on a call overrides the environment variable. The fallback is used only when `/sys.scripts.do` certainly did not run the script: HTTP 401, 403 or 404, a login page, a "not authorized" answer, or a connection error before sending. The tool description gains one sentence: "The sys_trigger fallback is opt-in (allow_trigger_fallback or HAPPY_MCP_ALLOW_TRIGGER_FALLBACK=1)."

A call with a `scope` argument never falls back to the scheduled job, even when the fallback is allowed: the job cannot run in an application scope. If the processor did not run such a script, the call stops and says the script was not run. With `wait: false`, `scope` is refused.

**SN-Set-Update-Set.** The tool changes the update set through the UI picker. If that change was sent and the answer was 408 or 5xx, 7.0 writes no fix script and does not say the change is unavailable. The update set may already be current. The message says so and tells you how to check (SN-Get-Current-Update-Set, or the picker in the UI after a browser refresh). If the change was accepted but the check afterwards reads back a different update set, the message names the set the instance reports (for example `'Default'`) and the fix script is written, as in 6.2.0; the audit record says "unknown". If the call fails before anything was sent, or ServiceNow refuses it clearly (a 4xx answer other than 408), you still get the 6.2.0 fix script.

**Client cancel stops a REST request.** When the client cancels a call, 7.0 stops the REST request that is running for it. If the request has not been sent yet, nothing was sent. If it was already sent, the outcome is unknown: the request may have been applied. The audit record says "unknown" in that case. A cancelled request is never retried.

### 7. Encoded queries refuse `^` and control characters

Many tools put one of your arguments into a ServiceNow encoded query: record numbers, user and group names, catalog items, keywords and similar. In 6.2.0 a value such as `INC0010001^ORnumberSTARTSWITHINC` was pasted in as it was, so it could add conditions and change which records a tool acted on.

7.0 refuses these values before sending anything. The error is an invalid-parameters error such as:

```
incident_number must not contain '^' (it would add encoded-query conditions)
assigned_to must not contain line breaks or other control characters
```

Refused: `^`, line breaks, tabs and all other control characters, including the Unicode line and paragraph separators. Arguments that are meant to be an encoded query (the `query` argument of the list tools, SN-Query-Table and the like) are still sent as given.

### 8. SN-Create-Workflow: numeric transition endpoints are refused

In 6.2.0, a transition whose `from` or `to` was a number (an activity position) got past the input check. The tool then wrote seven records and crashed with `Cannot read properties of undefined`, leaving a half-built workflow.

7.0 refuses the call up front with an invalid-parameters error (code -32602) and writes nothing. Give `from` and `to` as strings: a declared activity `id`, a unique activity name, or an existing activity sys_id.

### 9. Smaller changes

- **Number-like text is accepted where a number is expected** (for example `"2"` or `"1e3"`), and the boolean-like values that 6.2.0 accepted where an object is expected are still accepted. Text with spaces, line breaks or a `+` sign, such as `" 2"`, `"2\n"` or `"+2"`, is refused.
- **Connection errors name the cause.** The tool text keeps the transport code and the underlying reason (for example a connect timeout) instead of only "error sending request".
- **`servicenow://instance`** reports the number of tools this server actually lists: 60, or fewer in docs-only mode. The published 6.2.0 package always reported 6.
- **`--version`:** `happy-platform-mcp --version` (or `-V`), `servicenow-mcp-server --version` and `happy-platform-mcp-http --version` print the version and exit 0. 6.2.0 answered "Unknown command" with exit code 2.
- **No usable keychain:** when the system keychain cannot be opened (for example a headless session or an isolated `HOME`), SN-Register-Instance and credential lookups report `KEYCHAIN_UNAVAILABLE` rather than "credential not found". Nothing is written; set the credentials through the environment or run where the keychain is available.
- **Scope mismatch now fails.** `SN-Execute-Background-Script` with a `scope` argument checks the scope that the script really ran in. If it ran in another scope, the result is `ok: false`, `status: "scope_mismatch"` and `scopeMismatch: {"requested": ..., "ran": ...}`. 6.2.0 reported success.
- **Stricter table and sys_id checks.** `SN-Query-Table`, `SN-Natural-Language-Search`, `SN-Get-Record`, `SN-Create-Record` and `SN-Update-Record` refuse a `table_name` that is not 1-80 letters, digits or underscores, with an invalid-parameters error, before anything is sent. `SN-Get-Record` and `SN-Update-Record` also refuse a `sys_id` that is not 32 lowercase hexadecimal characters. 6.2.0 sent such values to ServiceNow and relayed its error.
- **Failed ATF tests say why.** When a test result is `failure` or `error`, `SN-Get-ATF-Results` and `SN-Run-ATF-Test` add `failing_steps` (up to 5 rows from `sys_atf_test_result_step`) and `failure_message` (the first message found). This costs one extra read, on failed tests only. If that read fails, the result carries `failing_steps_error` and the call still succeeds. `start_time` in a step row is in the instance's display time zone, without a zone marker.
- **The instance URL in `servicenow://instance` has no login part.** If the configured URL has a `user:password@` part, it is not shown. A URL whose `@` appears only in the path or query (and has no `:` before it) is shown unchanged. When the text is ambiguous (for example an unencoded `/` or `?` in a password), everything up to the last `@` is dropped. If no host is left, nothing is shown.
- **Fix scripts are written under the start-up directory.** 7.0 writes `scripts/<file>.js` under the directory the server was started in, not wherever the current directory happens to be at that moment. If that directory cannot be resolved (for example it was deleted), the server still starts, and the tool answers with the script text inline and writes no file.

## Things that were removed or work differently

### 10. script-sync is dropped

The 6.2.0 package contained `script-sync.js` (sync and watch for scripts). It was never a tool and nothing called it. 7.0 does not include it. If you used it directly from the 6.x package, stay on 6.x for that.

### 11. Usage telemetry counts registered tool names only

Telemetry is still off by default. You turn it on with `HAPPY_MCP_TELEMETRY=1` (also `true`, `on`, `yes`). When it is on, the server prints one line saying so and where it sends data (on stderr for stdio, on stdout for HTTP).

One thing changed. 6.2.0 counted every name a client sent, including names the server did not know, so a client could put its own text into the report. 7.0 counts a name only after the server has found that tool. Unknown names are not counted at all. The report still contains only tool names and counts, a random session id, version, platform and transport. It never contains arguments, results, instance URLs or user names.

Other telemetry details that differ from 6.2.0: an override URL (`HAPPY_MCP_TELEMETRY_URL`) must be https, or plain http to the local machine only; any other value switches telemetry off and prints a note; redirects are not followed; the startup line shows the override URL without user name, password or query; and a send takes at most 3 seconds and is never retried.

### 12. `.env` files: a short list of keys is loaded

6.2.0 read a `.env` file in the working directory at start-up and took every key in it. 7.0 reads the same file, but only a fixed list of keys, and only on the server paths: the stdio server and the HTTP server. The `instance` commands and `--help` do not read it.

- **Where:** `.env` in the working directory of the process.
- **Which keys are loaded:** `SERVICENOW_INSTANCE`, `SERVICENOW_INSTANCE_URL`, `SERVICENOW_AUTH_TYPE`, `SERVICENOW_USERNAME`, `SERVICENOW_PASSWORD`, `SERVICENOW_CLIENT_ID`, `SERVICENOW_CLIENT_SECRET`, `SERVICENOW_OAUTH_GRANT_TYPE` and `HAPPY_MCP_API_TOKEN`.
- **Every other key is ignored**, even if 6.2.0 used it: the bind host, the allowed hosts and origins, the telemetry switch and URL, `HAPPY_CONFIG_PATH`, `PORT` and so on. Set those in the real environment.
- **The real environment wins.** A key that is already set in the process environment is not replaced by `.env`, even if its value is empty.
- **The file must be:** a regular file (a symlink to one is followed), at most 64 KiB, valid UTF-8. A UTF-8 byte-order mark at the start is stripped. Anything else is not loaded at all and the server prints one warning.
- **Warnings go to stderr, never stdout.** A malformed line is reported by its line number only; the line text is never printed, so a secret in it is not echoed. If `SERVICENOW_INSTANCE_URL` comes from `.env`, the server also prints a note that `happy-platform-mcp instance add` is the better place for it.

If you keep only `SERVICENOW_*` credentials or `HAPPY_MCP_API_TOKEN` in `.env`, nothing changes for you. If you keep any other setting in `.env`, move it to the process environment (service manager, container or shell) before you upgrade.

### 13. Local docs index after a downgrade

7.0 reads the same local docs index as 6.x (`index.sqlite`) and keeps its own vector index in it. If you go back to 6.x after 7.0 re-synced a docs family, semantic (vector) results for that family can be missing under 6.x. Run `SN-Docs-Sync` again under 6.x for those families. Keyword search is not affected.

### 14. Set-Instance and registration

- `SN-Set-Instance` switches the default instance for the running server. On HTTP, all sessions of the same server share that choice, as in 6.2.0.
- `SN-Register-Instance` in a docs-only server reports that a restart is needed.
- Instances registered by an agent follow the credential rules in section 2.

### 15. Protocol answers that differ from 6.2.0

These are about the MCP wire protocol. Normal clients do not notice them.

- **Unknown resource address (SEP-2164).** 6.2.0 answered `resources/read` of an unknown URI with JSON-RPC `-32603` (internal error). 7.0 answers `-32602` (invalid params) with the URI in `data.uri`, on every protocol version.
- **Batches are refused.** A JSON-RPC batch (an array of requests) gets `-32600` ("Batch requests are not supported") on stdio, streamable HTTP and the legacy SSE transport.
- **`prompts/list`.** The server declares no prompts. A client on the new protocol version (`2026-07-28`) gets `-32601` (method not found), as 6.2.0 always answered. Older clients get an empty list.
- **Unknown session id.** A request that carries a session id the server does not know (or has not bound) gets HTTP 404.
- **Connections that never authenticate.** An HTTP connection must send an authenticated request within 10 seconds or it is closed. One remote address may hold at most 16 connections that have not authenticated yet; a new one evicts the oldest. IPv6 addresses count by /64. `X-Forwarded-For` is never trusted, so behind a reverse proxy all clients share the proxy's address. Connections that did authenticate (a proxy's keep-alive pool) count only against the overall connection cap.
- **Legacy SSE.** Closing the event stream does not cancel running tool calls. A `notifications/cancelled` message does.

### 16. Start-up messages and docs-only mode

- The stdio server prints the same start-up lines on stderr as 6.2.0: the default instance, "started", the mode and instance, and the release-notes hint. An instance URL with a `user:password@` part is refused when the configuration loads, so these lines never show one.
- Two 6.2.0 stderr lines are **not** printed in 7.0: `✅ Loaded metadata for 94 ServiceNow tables` and the per-request `📋 Tool list requested…` lines. They were diagnostics only. Nothing reads them on stdout, and stdout is unchanged.
- `HAPPY_MCP_DOCS_ONLY` applies to the stdio server only, as in 6.2.0. The HTTP server does not read it.
- Claude Code skips the `oneOf` form of the `SN-Register-Instance` schema. This is the same as with 6.2.0; the schema is unchanged.

### 17. Known risk we accept for now

When 7.0 saves a credential it deletes the old keychain item and then creates a new one. The `keyring` library has no create-only write (the macOS `SecItemAdd` call that fails if the item exists). So there is a short gap between delete and create. A local program running as the same user could create the item in that gap, and 7.0 would then write into an item that program made. We accept this for 7.0. The fix is a create-only write, planned for a later release.
