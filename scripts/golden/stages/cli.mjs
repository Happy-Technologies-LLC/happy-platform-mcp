// Stage: CLI behaviour (both bin names), help/usage, secret-argument rejection,
// every reachable exit code, interactive flows with injected prompts.
//  - Non-interactive cases run the real bin symlink (shebang) as a subprocess, stdin=/dev/null.
//  - Interactive cases run lib/cli-runner.mjs (dispatch() with injected prompts) under the same preload.
// Registry = temp HAPPY_CONFIG_PATH; credentials = fake keyring file. No real keychain, no real hosts.
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { REPO, CLI_JS, PRELOAD, GOLDEN_DIR } from '../lib/paths.mjs';
import { assertSafeSpawn } from '../lib/guard.mjs';
import { makeSandbox, refreshScrubber, FAKE_PASSWORD } from '../lib/world.mjs';

const BINS = ['happy-platform-mcp', 'servicenow-mcp-server'];
const U = n => `${n.toString(16).padStart(32, 'a')}`;
// Sentinel the CLI must refuse as a credential argument; never a real credential.
const ARGV_SENTINEL = 'hunter2-golden';

const legacyRegistry = world => ({
  version: 1,
  instances: [
    { name: 'dev', url: world.stub.origin, username: 'golden.user', password: FAKE_PASSWORD, description: 'Golden dev', default: true },
    { name: 'oauth1', url: world.stub.origin, authType: 'oauth', grantType: 'client_credentials', clientId: 'golden-client', clientSecret: 'golden-fake-client-secret', description: 'Golden oauth' }
  ]
});
const refRegistry = world => ({
  version: 1,
  instances: [
    { name: 'dev', url: world.stub.origin, authType: 'basic', username: 'golden.user', credentialRef: 'keychain:instance/dev/password', description: 'Golden dev', default: true },
    { name: 'oauth1', url: world.stub.origin, authType: 'oauth', grantType: 'client_credentials', clientId: 'golden-client', credentialRef: 'keychain:instance/oauth1/client-secret', description: 'Golden oauth' }
  ]
});
const refKeyring = { 'keychain:instance/dev/password': FAKE_PASSWORD, 'keychain:instance/oauth1/client-secret': 'golden-fake-client-secret' };

function s(id, argv, extra = {}) { return { id, argv, ...extra }; }

// ---- subprocess scenarios (bin symlink) ------------------------------------
const both = (name, argv, extra) => BINS.map(bin => ({ id: `${name}`, bin, argv, ...extra }));
export const subprocessScenarios = [
  ...both('help-long', ['--help']),
  ...both('help-short', ['-h']),
  ...both('help-word', ['help']),
  ...both('instance-bare', ['instance']),
  ...both('instance-help-long', ['instance', '--help']),
  ...both('instance-help-short', ['instance', '-h']),
  ...both('instance-help-word', ['instance', 'help']),
  ...both('unknown-command', ['frobnicate']),
  ...both('unknown-flag-only', ['--no-such-flag']),
  ...both('unknown-instance-subcommand', ['instance', 'frobnicate']),
  ...both('secret-flag-space', ['instance', 'add', '--password', ARGV_SENTINEL]),
  ...both('secret-flag-equals', ['instance', 'add', `--password=${ARGV_SENTINEL}`]),
  ...both('secret-flag-client-secret', ['instance', 'update', 'dev', '--client-secret', 'x-golden']),
  ...both('secret-flag-token', ['instance', 'credential', 'set', 'dev', '--token', 'x-golden']),
  ...both('secret-assignment-form', ['instance', 'add', `password=${ARGV_SENTINEL}`]),
  ...both('secret-flag-top-level', ['--password', 'x-golden']),
  ...both('credential-type-unsafe', ['instance', 'credential', 'set', 'dev', '--type', 'bogus']),
  s('list-empty-registry', ['instance', 'list'], { bin: BINS[0], registry: () => ({ version: 1, instances: [] }) }),
  s('list-no-registry-file', ['instance', 'list'], { bin: BINS[0] }),
  s('list-legacy-plaintext-registry', ['instance', 'list'], { bin: BINS[0], registry: legacyRegistry }),
  s('list-credential-ref-registry', ['instance', 'list'], { bin: BINS[0], registry: refRegistry, keyring: refKeyring }),
  s('list-registry-malformed-json', ['instance', 'list'], { bin: BINS[0], registryRaw: '{ not json' }),
  s('list-extra-args', ['instance', 'list', 'extra'], { bin: BINS[0] }),
  s('test-no-name', ['instance', 'test'], { bin: BINS[0], registry: refRegistry, keyring: refKeyring }),
  s('test-unknown-instance', ['instance', 'test', 'nope'], { bin: BINS[0], registry: refRegistry, keyring: refKeyring }),
  s('test-success', ['instance', 'test', 'dev'], { bin: BINS[0], registry: refRegistry, keyring: refKeyring }),
  s('test-http-401', ['instance', 'test', 'dev'], { bin: BINS[0], registry: refRegistry, keyring: refKeyring, stub: ({ req }) => req.path.startsWith('/api/now/table') ? [401, { error: { message: 'User Not Authenticated' } }] : null }),
  s('test-http-403', ['instance', 'test', 'dev'], { bin: BINS[0], registry: refRegistry, keyring: refKeyring, stub: ({ req }) => req.path.startsWith('/api/now/table') ? [403, { error: { message: 'User Not Authorized' } }] : null }),
  s('test-credential-missing', ['instance', 'test', 'dev'], { bin: BINS[0], registry: refRegistry }),
  s('add-non-tty', ['instance', 'add'], { bin: BINS[0] }),
  s('remove-non-tty', ['instance', 'remove', 'dev'], { bin: BINS[0], registry: refRegistry, keyring: refKeyring }),
  s('credential-set-non-tty', ['instance', 'credential', 'set', 'dev', '--type', 'password'], { bin: BINS[0], registry: refRegistry }),
  s('migrate-non-tty', ['instance', 'migrate'], { bin: BINS[0], registry: legacyRegistry }),
  s('update-unknown-option', ['instance', 'update', 'dev', '--frobnicate', 'x'], { bin: BINS[0], registry: refRegistry, keyring: refKeyring }),
  s('credential-set-missing-name', ['instance', 'credential', 'set'], { bin: BINS[0] }),
  s('remove-extra-args', ['instance', 'remove', 'a', 'b'], { bin: BINS[0] })
];

// ---- in-process interactive scenarios (dispatch + injected prompts) --------
export const interactiveScenarios = [
  s('add-basic', ['instance', 'add'], { answers: { name: 'staging', url: 'http://127.0.0.1:1', authType: 'basic', username: 'golden.user', description: '' }, registry: refRegistry, keyring: refKeyring }),
  s('add-duplicate-name', ['instance', 'add'], { answers: { name: 'dev', url: 'http://127.0.0.1:1', authType: 'basic', username: 'golden.user' }, registry: refRegistry, keyring: refKeyring }),
  s('add-invalid-url', ['instance', 'add'], { answers: { name: 'bad', url: 'ftp://nope.invalid', authType: 'basic', username: 'golden.user' }, registry: refRegistry, keyring: refKeyring }),
  s('add-oauth-client-credentials', ['instance', 'add'], { answers: { name: 'oauth2', url: 'http://127.0.0.1:1', authType: 'oauth', grantType: 'client_credentials', clientId: 'golden-client-2', scope: '' }, registry: refRegistry, keyring: refKeyring }),
  s('add-into-empty-registry-becomes-default', ['instance', 'add'], { answers: { name: 'first', url: 'http://127.0.0.1:1', authType: 'basic', username: 'golden.user' }, registry: () => ({ version: 1, instances: [] }) }),
  s('update-metadata-flags', ['instance', 'update', 'dev', '--description', 'Golden updated', '--no-default'], { registry: refRegistry, keyring: refKeyring }),
  s('update-prompted', ['instance', 'update', 'dev'], { answers: { description: 'Golden prompted update' }, registry: refRegistry, keyring: refKeyring }),
  s('update-unknown-instance', ['instance', 'update', 'nope', '--description', 'x'], { registry: refRegistry, keyring: refKeyring }),
  s('update-auth-change-rejected', ['instance', 'update', 'dev', '--auth-type', 'oauth'], { registry: refRegistry, keyring: refKeyring }),
  s('credential-set-password', ['instance', 'credential', 'set', 'dev', '--type', 'password'], { registry: refRegistry }),
  s('credential-set-client-secret', ['instance', 'credential', 'set', 'oauth1', '--type', 'client-secret'], { registry: refRegistry }),
  s('credential-set-type-prompted', ['instance', 'credential', 'set', 'dev'], { answers: { type: 'password' }, registry: refRegistry }),
  s('credential-set-wrong-type-for-auth', ['instance', 'credential', 'set', 'dev', '--type', 'client-secret'], { registry: refRegistry }),
  s('credential-set-unknown-instance', ['instance', 'credential', 'set', 'nope', '--type', 'password'], { registry: refRegistry }),
  s('remove-confirmed', ['instance', 'remove', 'oauth1'], { answers: { confirm: true }, registry: refRegistry, keyring: refKeyring }),
  s('remove-declined', ['instance', 'remove', 'oauth1'], { answers: { confirm: false }, registry: refRegistry, keyring: refKeyring }),
  s('remove-unknown-instance', ['instance', 'remove', 'nope'], { answers: { confirm: true }, registry: refRegistry, keyring: refKeyring }),
  s('migrate-confirmed', ['instance', 'migrate'], { answers: { confirm: true }, registry: legacyRegistry, migrateTarget: true }),
  s('migrate-declined', ['instance', 'migrate'], { answers: { confirm: false }, registry: legacyRegistry, migrateTarget: true }),
  s('migrate-nothing-to-do', ['instance', 'migrate'], { answers: { confirm: true }, registry: refRegistry, keyring: refKeyring, migrateTarget: true }),
  s('migrate-source-equals-target', ['instance', 'migrate'], { answers: { confirm: true }, registry: legacyRegistry })
];

function prepare(world, scenario, nameSuffix) {
  const box = makeSandbox(world, `cli-${scenario.id}-${nameSuffix}`, { withCreds: false });
  delete box.env.HAPPY_CONFIG_PATH;
  const registryPath = path.join(box.dir, 'cfg', 'instances.json');
  if (scenario.registry) fs.writeFileSync(registryPath, JSON.stringify(scenario.registry(world), null, 2));
  if (scenario.registryRaw !== undefined) fs.writeFileSync(registryPath, scenario.registryRaw);
  box.env.HAPPY_CONFIG_PATH = registryPath;
  if (scenario.migrateTarget) {
    // legacy plaintext source is read from the package-legacy path; target is a distinct user registry.
    const legacy = path.join(box.dir, 'legacy.json');
    fs.copyFileSync(registryPath, legacy);
    fs.rmSync(registryPath);
    box.legacyPath = legacy;
  }
  if (scenario.keyring) {
    const seeded = Object.fromEntries(Object.entries(scenario.keyring).map(([account, secret]) => [`happy-platform-mcp\u0000${account}`, secret]));
    fs.writeFileSync(box.env.GOLDEN_KEYRING_FILE, JSON.stringify(seeded), { mode: 0o600 });
  }
  box.registryPath = registryPath;
  return box;
}

// '=' inside secret-shaped tokens is rendered as {{EQ}} so that fixtures never contain a literal `password=`-style pair.
function displayArgv(argv) {
  return argv.map(token => token.replace(/^(-{0,2}[\w-]*(?:password|secret|token)[\w-]*)=(.*)$/i, '$1{{EQ}}<value>'));
}

function snapshot(box) {
  const state = {};
  state.registryFile = fs.existsSync(box.registryPath) ? fs.readFileSync(box.registryPath, 'utf8') : null;
  try { state.registryJson = state.registryFile ? JSON.parse(state.registryFile) : null; } catch { state.registryJson = 'unparseable'; }
  if (state.registryFile !== null && state.registryJson === 'unparseable') state.registryFile = '<malformed, preserved>';
  else delete state.registryFile;
  if (box.legacyPath) state.legacyFileUntouched = fs.existsSync(box.legacyPath);
  let keyring = {};
  try { keyring = JSON.parse(fs.readFileSync(box.env.GOLDEN_KEYRING_FILE, 'utf8')); } catch { /* none */ }
  state.keyringAccounts = Object.keys(keyring).map(k => k.split('\u0000')[1]).sort();
  state.keyringValueMatchesFakeSecret = Object.fromEntries(Object.entries(keyring).map(([k, v]) => [k.split('\u0000')[1], v === FAKE_PASSWORD || v === 'golden-fake-client-secret']).sort(([a], [b]) => (a < b ? -1 : 1)));
  return state;
}

// Async spawn: the loopback stub lives in this process, so the event loop must stay free.
function spawnCollect(command, args, { env, cwd, timeoutMs = 60000 }) {
  assertSafeSpawn({ cwd, env });
  return new Promise(resolve => {
    const child = spawn(command, args, { env, cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', c => { stdout += c; });
    child.stderr.on('data', c => { stderr += c; });
    const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMs);
    child.on('close', (status, signal) => { clearTimeout(timer); resolve({ status, signal, stdout, stderr }); });
  });
}

async function runBin(world, scenario) {
  const box = prepare(world, scenario, scenario.bin);
  const binDir = path.join(box.dir, 'bin');
  fs.mkdirSync(binDir, { recursive: true });
  const link = path.join(binDir, scenario.bin);
  fs.symlinkSync(CLI_JS, link);
  const env = { ...box.env, NODE_OPTIONS: `--import=${PRELOAD}` };
  world.stub.reset(scenario.stub || null);
  const run = await spawnCollect(link, scenario.argv, { env, cwd: box.work });
  return {
    id: `cli/${scenario.id}`,
    bin: scenario.bin,
    mode: 'subprocess-bin-symlink, stdin=/dev/null (non-TTY)',
    argv: displayArgv(scenario.argv),
    exit: { code: run.status, signal: run.signal },
    stdout: run.stdout,
    stderr: run.stderr,
    outbound: world.stub.take().map(r => ({ target: 'servicenow', ...r })),
    after: snapshot(box)
  };
}

async function runInteractive(world, scenario) {
  const box = prepare(world, scenario, 'interactive');
  const spec = { repo: REPO, argv: scenario.argv, answers: scenario.answers || {} };
  // package-legacy -> user-registry migration needs readPath != writePath (src/config-path.js:33-35).
  if (box.legacyPath) spec.registryOptions = { readPath: box.legacyPath, writePath: box.registryPath };
  const env = { ...box.env, GOLDEN_CLI_SPEC: JSON.stringify(spec) };
  world.stub.reset(scenario.stub || null);
  const run = await spawnCollect(process.execPath, ['--import', PRELOAD, path.join(GOLDEN_DIR, 'lib', 'cli-runner.mjs')], { env, cwd: box.work });
  let result;
  try { result = JSON.parse(run.stdout); } catch { result = { harnessError: `runner output unparseable`, stdout: run.stdout, stderr: run.stderr }; }
  return {
    id: `cli/interactive/${scenario.id}`,
    mode: 'in-process dispatch(argv, {prompts injected}); real registry file + real credential store over fake keyring',
    argv: displayArgv(scenario.argv),
    answers: scenario.answers || {},
    exit: { code: result.code },
    stdout: result.stdout,
    stderr: result.stderr,
    promptCalls: result.promptCalls,
    runnerError: result.harnessError || null,
    outbound: world.stub.take().map(r => ({ target: 'servicenow', ...r })),
    after: snapshot(box)
  };
}

export async function runCli(world) {
  const out = new Map();
  for (const scenario of subprocessScenarios) {
    refreshScrubber(world);
    const doc = await runBin(world, scenario);
    const dup = scenario.bin === BINS[1] ? `${scenario.id}.${scenario.bin}` : scenario.bin === BINS[0] && BINS.length > 1 && subprocessScenarios.some(x => x.id === scenario.id && x.bin === BINS[1]) ? `${scenario.id}.${scenario.bin}` : scenario.id;
    out.set(path.posix.join('cli', `${dup}.json`), world.scrubber.deep(doc));
  }
  for (const scenario of interactiveScenarios) {
    refreshScrubber(world);
    const doc = await runInteractive(world, scenario);
    out.set(path.posix.join('cli', 'interactive', `${scenario.id}.json`), world.scrubber.deep(doc));
  }
  return out;
}
