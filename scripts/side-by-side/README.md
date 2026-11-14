# Side-by-side harness (6.2.0 vs the Rust port)

Runs `happy-platform-mcp@6.2.0` (Node, bin `servicenow-mcp-server`) and the Rust
`servicenow-mcp-server` as two stdio MCP children against **one** ServiceNow PDI
(`dev451274`), drives the same JSON-RPC sequence into both, normalises volatile
values, diffs, and classifies every difference. Developer tool for the 7.0 `next`
cycle (P5-5); not part of CI and not shipped.

## Run

```sh
cargo build --release -p hpm-cli --locked   # package renamed to `happy-platform-mcp` on p5-release: use that name there
node scripts/side-by-side/run.mjs                       # full run (read set + write set)
node scripts/side-by-side/run.mjs --phase reads         # read set only, writes nothing
```

Options: `--out DIR` (default `/tmp/hpm-p5-sbs`), `--js-prefix DIR` (an npm prefix
with `happy-platform-mcp@6.2.0`; created with `--ignore-scripts` when missing; its
`@napi-rs/keyring` does the keychain read), `--rust-bin PATH` (default
`target/release/servicenow-mcp-server`), `--run-id ID` (`[a-z0-9]{4,12}`),
`--js-mode npx|installed` (default `npx -y -p happy-platform-mcp@6.2.0 servicenow-mcp-server`).

Needs macOS with the `dev451274` password in the keychain (service
`happy-platform-mcp`, account `keychain:instance/dev451274/password`) and a PDI
that is awake. If the PDI is unreachable or hibernating the run stops
(exit 2) and tries nothing else. Exit 0 = run complete and every check true;
3 = a check failed (leak, cleanup, before/after mismatch).

On macOS the host firewall (LuLu) may block a new binary path (P4 note F1): allow
the Rust binary once if the first call times out with a Connect error.

## Safety model

- **Pinned to dev451274.** Children get `SERVICENOW_INSTANCE_URL/USERNAME/PASSWORD`
  from the environment under an isolated `HOME`/`XDG_CONFIG_HOME` (no registry, so no
  `ccfplg` entry exists for them). Before anything else both report their instance;
  the run aborts unless the host is `dev451274.service-now.com`. `sn.mjs` pins the
  host too and only allows `GET`, and `DELETE /api/now/table/<table>/<sys_id>`.
- **Secrets.** The password is read once, passed only through the child env, every
  logged line and child output is scrubbed, and `leaks` counts scrub hits (must be 0).
  Never put the password on a command line.
- **Closed tool set.** Only the tools in `READ_TOOLS` / `WRITE_TOOLS` can be called.
  No update-set, scope, property, script, ATF, catalog or instance-switch tools. Write
  tools may only target `incident`, only records the calling server created, and
  every payload must carry that server's prefix.
- **Isolation.** Each server writes only records named `hpm-p5-sbs-<runid>-js` /
  `-rs`; they never touch each other's records.
- **Cleanup.** Created incidents are found by prefix over REST and deleted; the run
  then verifies prefix count 0, incident total unchanged, `sys_update_xml` total
  unchanged and none created since the run started, the admin update-set preference
  unchanged, registry sha256 and keychain account names (names only) unchanged. A
  crash triggers a best-effort prefix cleanup.

## Output (`<out>/run-<runid>/`)

`results.json` (meta, checks, steps), `table.md` (step table), `steps/<id>.json`
(normalised both sides + every diff line), `transcript-{js,rs}.jsonl`,
`stderr-{js,rs}.log`, `snapshot-{before,after}.json`. Files are mode 0600.

## Classification

`match`; `expected-diff` (matched by a rule that cites a recorded deviation:
`crates/hpm-tools/tests/support/deviations.rs`, `crates/hpm-tools/tests/golden_surface.rs`
`DEVIATIONS`, `fixtures/tools-list-*.json`, `p4/notes-pending.md`,
`7.0-migration-notes.md`); `unexpected-diff` (anything else: report it, do not
paper over it by adding a rule); `info-diff` (stderr diagnostics only).
For `tools/list` a tool counts as expected only if the 6.2.0 side equals
`fixtures/tools-list-6.2.0.json` and the Rust side equals `fixtures/tools-list-7.0.json`.
