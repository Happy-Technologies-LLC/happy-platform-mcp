#!/usr/bin/env node
/**
 * Compatibility-manifest drift checker (plan §7.1, D-02).
 *
 * Re-derives every machine-checkable part of fixtures/compat-manifest.json from
 * the Node baseline sources and exits non-zero on any difference:
 *
 *   package   bin names/targets, name, version
 *   tools     names, order, input-schema + description hashes (full 60-tool list and the
 *             docs-only list), through a real MCP client over the SDK's in-memory transport
 *   protocol  initialize result (serverInfo, capabilities), resources/list, prompts/list,
 *             registered request methods
 *   env       static scan of src/ for every environment variable read
 *   cli       usage text, instance subcommands, metadata flags, secret-argument rule,
 *             and child-process exit-code probes (isolated HOME, no network, no keychain)
 *   startup   stdio docs-only / fallback decision matrix (injected env + config manager)
 *   config    resolveConfigPaths precedence matrix
 *   keychain  service/account formats, identity-v1 tuple source + test vectors, token file names
 *   sources   every `source` "file:line[-line]" anchor (+ optional `sourceContains`) in the manifest
 *
 * Source-anchor drift is an error only with --strict-sources (line numbers move when the
 * baseline is patched); everything else is always an error.
 *
 * Usage: node scripts/compat/check-manifest.mjs [--root DIR] [--manifest FILE]
 *          [--fixture FILE] [--strict-sources] [--emit]
 *   --root      tree to check (default: repo root of this script); src/ is read from here
 *   --manifest  default <root>/fixtures/compat-manifest.json
 *   --fixture   tools/list fixture to compare against (also env HPM_TOOLS_LIST_FIXTURE);
 *               when omitted, <root>/fixtures/tools-list-6.2.0.json is used if present
 *   --emit      print the derived tools/env blocks as JSON and exit 0 (for regenerating)
 *
 * No dependencies beyond the repo's own node_modules.
 */
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));

function parseArgs(argv) {
  const opts = { root: path.resolve(SCRIPT_DIR, '..', '..'), strictSources: false, emit: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${arg} requires a value`);
      return argv[++i];
    };
    if (arg === '--root') opts.root = path.resolve(value());
    else if (arg === '--manifest') opts.manifest = path.resolve(value());
    else if (arg === '--fixture') opts.fixture = path.resolve(value());
    else if (arg === '--strict-sources') opts.strictSources = true;
    else if (arg === '--emit') opts.emit = true;
    else throw new Error(`Unknown argument ${arg}`);
  }
  opts.manifest ||= path.join(opts.root, 'fixtures', 'compat-manifest.json');
  opts.fixture ||= process.env.HPM_TOOLS_LIST_FIXTURE
    ? path.resolve(process.env.HPM_TOOLS_LIST_FIXTURE)
    : path.join(opts.root, 'fixtures', 'tools-list-6.2.0.json');
  return opts;
}

// ---------------------------------------------------------------- reporting
const failures = [];
const warnings = [];
const sectionCounts = new Map();
let section = 'init';

function begin(name) {
  section = name;
  if (!sectionCounts.has(name)) sectionCounts.set(name, { checks: 0, failed: 0 });
}
function check(condition, message) {
  const counts = sectionCounts.get(section);
  counts.checks += 1;
  if (!condition) {
    counts.failed += 1;
    failures.push(`${section}: ${message}`);
  }
  return Boolean(condition);
}
function same(actual, expected, message) {
  const a = JSON.stringify(canonical(actual));
  const e = JSON.stringify(canonical(expected));
  return check(a === e, `${message}\n      expected ${truncate(e)}\n      actual   ${truncate(a)}`);
}
function truncate(text, max = 400) {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

// ---------------------------------------------------------------- helpers
export function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.keys(value).sort().map(key => [key, canonical(value[key])]));
  }
  return value;
}
const sha256 = text => crypto.createHash('sha256').update(text, 'utf8').digest('hex');
const hashJson = value => sha256(JSON.stringify(canonical(value)));

function readText(root, rel) {
  return fs.readFileSync(path.join(root, rel), 'utf8');
}
function walkJs(dir) {
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walkJs(full));
    else if (entry.isFile() && entry.name.endsWith('.js')) out.push(full);
  }
  return out.sort();
}
function stripComments(text) {
  return text
    .replace(/\/\*[\s\S]*?\*\//g, block => block.replace(/[^\n]/g, ' '))
    .split('\n')
    .map(line => line.replace(/(^|\s)\/\/.*$/, '$1'))
    .join('\n');
}

async function quiet(fn) {
  const original = console.error;
  console.error = () => {};
  try { return await fn(); } finally { console.error = original; }
}

// ---------------------------------------------------------------- env scan
export function scanEnvVars(root) {
  const found = new Map(); // name -> [file:line]
  const note = (name, file, line) => {
    if (!found.has(name)) found.set(name, []);
    found.get(name).push(`${file}:${line}`);
  };
  const srcDir = path.join(root, 'src');
  for (const file of walkJs(srcDir)) {
    const rel = path.relative(root, file).split(path.sep).join('/');
    const text = stripComments(fs.readFileSync(file, 'utf8'));
    const consts = new Map();
    for (const m of text.matchAll(/\bconst\s+([A-Za-z_]\w*)\s*=\s*['"]([A-Z][A-Z0-9_]+)['"]/g)) consts.set(m[1], m[2]);
    const lines = text.split('\n');
    lines.forEach((line, index) => {
      const lineNo = index + 1;
      for (const m of line.matchAll(/\benv\.([A-Z][A-Z0-9_]+)\b/g)) note(m[1], rel, lineNo);
      for (const m of line.matchAll(/\benv\[\s*['"`]([A-Z][A-Z0-9_]+)['"`]\s*\]/g)) note(m[1], rel, lineNo);
      for (const m of line.matchAll(/\benvHas\(\s*\w+\s*,\s*['"`]([A-Z][A-Z0-9_]+)['"`]/g)) note(m[1], rel, lineNo);
      for (const m of line.matchAll(/\bwithEnvName\(\s*['"`]([A-Z][A-Z0-9_]+)['"`]/g)) note(m[1], rel, lineNo);
      for (const m of line.matchAll(/\benv\[\s*([A-Za-z_]\w*)\s*\]/g)) {
        if (consts.has(m[1])) note(consts.get(m[1]), rel, lineNo);
      }
    });
  }
  return found;
}

// ---------------------------------------------------------------- source anchors
const SOURCE_KEYS_SKIPPED = new Set(['expect']); // resolveConfigPaths results carry their own `source` field

function collectSources(node, trail, out) {
  if (Array.isArray(node)) {
    node.forEach((item, i) => collectSources(item, `${trail}[${i}]`, out));
    return;
  }
  if (!node || typeof node !== 'object') return;
  if (typeof node.source === 'string') {
    const contains = node.sourceContains === undefined ? [] : [].concat(node.sourceContains);
    out.push({ trail, anchor: node.source, contains });
  }
  for (const [key, child] of Object.entries(node)) {
    if (!SOURCE_KEYS_SKIPPED.has(key)) collectSources(child, `${trail}.${key}`, out);
  }
}

/**
 * Anchor grammar: segments separated by ";", each `path[:range[,range]*]` where range is `N` or `N-M`
 * (a bare path only asserts the file exists). `sourceContains` applies when the anchor is one
 * single-range segment.
 */
function parseAnchor(anchor) {
  const segments = [];
  for (const raw of anchor.split(';')) {
    const text = raw.trim();
    const m = /^([\w./-]+?)(?::(\d+(?:-\d+)?(?:,\d+(?:-\d+)?)*))?$/.exec(text);
    if (!m) return null;
    const ranges = (m[2] ? m[2].split(',') : []).map(r => {
      const [a, b] = r.split('-');
      return [Number(a), Number(b || a)];
    });
    segments.push({ rel: m[1], ranges });
  }
  return segments;
}

function checkSources(manifest, root, strict) {
  begin('sources');
  const anchors = [];
  collectSources(manifest, '$', anchors);
  check(anchors.length > 0, 'manifest carries no source anchors');
  const drift = [];
  const cache = new Map();
  for (const { trail, anchor, contains } of anchors) {
    const segments = parseAnchor(anchor);
    if (!segments) { drift.push(`${trail}: malformed source "${anchor}"`); continue; }
    for (const { rel, ranges } of segments) {
      let lines = cache.get(rel);
      if (lines === undefined) {
        try { lines = readText(root, rel).split('\n'); } catch { lines = null; }
        cache.set(rel, lines);
      }
      if (!lines) { drift.push(`${trail}: source file missing "${rel}"`); continue; }
      for (const [start, end] of ranges) {
        if (start < 1 || end < start || end > lines.length) drift.push(`${trail}: line range ${start}-${end} outside ${rel} (${lines.length} lines)`);
      }
    }
    if (contains.length && segments.length === 1 && segments[0].ranges.length === 1) {
      const [{ rel, ranges: [[start, end]] }] = segments;
      const body = (cache.get(rel) || []).slice(start - 1, end).join('\n');
      for (const needle of contains) {
        if (!body.includes(needle)) drift.push(`${trail}: "${anchor}" no longer contains ${JSON.stringify(needle)}`);
      }
    }
  }
  if (strict) {
    check(drift.length === 0, `source anchor drift:\n      ${drift.join('\n      ')}`);
  } else {
    check(true, '');
    if (drift.length) {
      warnings.push(`sources: ${drift.length} anchor(s) drifted (use --strict-sources to fail):\n      ${drift.slice(0, 20).join('\n      ')}`);
    }
  }
}

// ---------------------------------------------------------------- live derivation
async function importFrom(root, rel) {
  return import(pathToFileURL(path.join(root, rel)).href);
}

async function deriveProtocol(root) {
  const { Client } = await import('@modelcontextprotocol/sdk/client/index.js');
  const { InMemoryTransport } = await import('@modelcontextprotocol/sdk/inMemory.js');
  const { createMcpServer } = await importFrom(root, 'src/mcp-server-consolidated.js');

  const configManager = {
    registry: {},
    getInstance() { throw new Error('not used'); },
    listInstances() { return []; }
  };
  const stubClient = { setProgressCallback() {}, getCurrentInstance() { return { name: 'x' }; } };
  const deps = { configManager, credentialStore: {}, usageTracker: { record() {} } };

  async function session(server) {
    const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
    const client = new Client({ name: 'compat-check', version: '0' }, { capabilities: {} });
    await Promise.all([server.connect(serverSide), client.connect(clientSide)]);
    return client;
  }

  const result = {};
  const full = await quiet(() => createMcpServer(stubClient, deps));
  const client = await session(full);
  result.serverInfo = client.getServerVersion();
  result.capabilities = client.getServerCapabilities();
  result.tools = (await client.listTools()).tools;
  result.resources = (await client.listResources()).resources;
  try {
    await client.listPrompts();
    result.promptsList = { ok: true };
  } catch (error) {
    result.promptsList = { ok: false, code: error.code };
  }
  try {
    await client.readResource({ uri: 'servicenow://nope' });
    result.unknownResource = { ok: true };
  } catch (error) {
    result.unknownResource = { ok: false, code: error.code, message: error.message.replace(/^MCP error -?\d+: /, '') };
  }
  result.requestMethods = [...full._requestHandlers.keys()].sort();
  await client.close();

  const docsServer = await quiet(() => createMcpServer(null, { docsOnly: true, ...deps }));
  const docsClient = await session(docsServer);
  result.docsOnlyTools = (await docsClient.listTools()).tools;
  const unknown = await quiet(() => docsClient.callTool({ name: 'SN-Nope', arguments: {} }));
  result.docsOnlyUnknownTool = unknown;
  await docsClient.close();
  return result;
}

function toolDigest(tool) {
  return {
    name: tool.name,
    inputSchemaSha256: hashJson(tool.inputSchema),
    descriptionSha256: sha256(tool.description ?? '')
  };
}

// ---------------------------------------------------------------- sections
function checkPackage(manifest, root) {
  begin('package');
  const pkg = JSON.parse(readText(root, 'package.json'));
  same(pkg.bin, manifest.cli.bins, 'package.json bin map');
  check(pkg.name === manifest.baseline.package, `package name ${pkg.name} != ${manifest.baseline.package}`);
  check(pkg.version === manifest.baseline.version, `package version ${pkg.version} != ${manifest.baseline.version}`);
  check(pkg.type === 'module', 'package.json type is not "module"');
  check(pkg.dependencies?.['@modelcontextprotocol/sdk'] === manifest.baseline.mcpSdk,
    `MCP SDK pin ${pkg.dependencies?.['@modelcontextprotocol/sdk']} != ${manifest.baseline.mcpSdk}`);
}

function checkTools(manifest, proto, opts) {
  begin('tools');
  const items = manifest.tools.items;
  check(manifest.tools.count === items.length, `manifest tools.count ${manifest.tools.count} != items ${items.length}`);
  same(proto.tools.map(t => t.name), items.map(t => t.name), 'registered tool names/order vs manifest');
  const live = new Map(proto.tools.map(t => [t.name, toolDigest(t)]));
  for (const item of items) {
    const digest = live.get(item.name);
    if (!digest) continue;
    check(digest.inputSchemaSha256 === item.inputSchemaSha256, `${item.name}: input schema hash drifted (${digest.inputSchemaSha256} != ${item.inputSchemaSha256})`);
    check(digest.descriptionSha256 === item.descriptionSha256, `${item.name}: description hash drifted`);
  }
  same(proto.docsOnlyTools.map(t => t.name), items.filter(t => t.docsOnly).map(t => t.name), 'docs-only tool list (order preserved) vs manifest');

  // tools/list fixture: the manifest hashes must equal the fixture's, and the fixture file itself must be unmodified.
  if (fs.existsSync(opts.fixture)) {
    const raw = fs.readFileSync(opts.fixture);
    const fixture = JSON.parse(raw.toString('utf8'));
    check(crypto.createHash('sha256').update(raw).digest('hex') === manifest.tools.fixture.sha256, `fixture file sha256 differs from manifest (${opts.fixture})`);
    const byName = list => [...list].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    same(byName(fixture.tools.map(toolDigest)), byName(items.map(({ name, inputSchemaSha256, descriptionSha256 }) => ({ name, inputSchemaSha256, descriptionSha256 }))),
      'tools/list fixture digests (by name; fixture is name-sorted) vs manifest');
    same(byName(fixture.tools.map(toolDigest)), byName(proto.tools.map(toolDigest)), 'tools/list fixture digests vs live registration');
  } else {
    warnings.push(`tools: fixture ${opts.fixture} not found; fixture comparison skipped (manifest-vs-source check still ran)`);
  }
}

function checkProtocol(manifest, proto) {
  begin('protocol');
  same(proto.serverInfo, manifest.protocol.serverInfo, 'serverInfo');
  same(proto.capabilities, manifest.protocol.capabilities, 'capabilities (full object incl. prompts)');
  same(proto.resources, manifest.protocol.resources.map(({ uri, name, mimeType, description }) => ({ uri, name, mimeType, description })), 'resources/list');
  same(proto.promptsList, manifest.protocol.promptsList, 'prompts/list behaviour');
  same(proto.unknownResource, manifest.protocol.unknownResource, 'resources/read of an unknown URI');
  same(proto.requestMethods, manifest.protocol.requestMethods, 'registered JSON-RPC request methods');
  same({ isError: proto.docsOnlyUnknownTool.isError, text: proto.docsOnlyUnknownTool.content?.[0]?.text }, manifest.errorShape.docsOnlyUnavailableTool,
    'docs-only unavailable-tool error result');
}

function checkEnv(manifest, root) {
  begin('env');
  const found = scanEnvVars(root);
  const scanned = manifest.environment.filter(e => !e.implicit).map(e => e.name).sort();
  same([...found.keys()].sort(), scanned, 'environment variables read in src/ (static scan) vs manifest');
  for (const entry of manifest.environment) {
    check(typeof entry.purpose === 'string' && entry.purpose && 'default' in entry && typeof entry.source === 'string',
      `${entry.name}: entry needs purpose, default and source`);
    if (!entry.implicit && found.has(entry.name)) {
      const sites = found.get(entry.name);
      check(sites.some(site => site.startsWith(entry.source.split(':')[0])), `${entry.name}: manifest source file ${entry.source} not among scanned read sites ${sites.join(', ')}`);
    }
  }
}

function checkCli(manifest, root) {
  begin('cli');
  const cliSrc = readText(root, 'src/instance-cli.js');
  const usageMatch = /const USAGE = `([\s\S]*?)`;/.exec(cliSrc);
  check(Boolean(usageMatch) && usageMatch[1] === manifest.cli.usage, 'USAGE text drifted');
  const cases = [...cliSrc.slice(cliSrc.indexOf('export async function runInstanceCli')).matchAll(/case '([a-z-]+)':/g)].map(m => m[1]);
  same(cases, manifest.cli.instanceSubcommands.map(s => s.name), 'instance subcommand switch cases');
  const flags = [...cliSrc.slice(cliSrc.indexOf('const METADATA_FLAGS'), cliSrc.indexOf('function output')).matchAll(/\['([a-z-]+)', '(\w+)'\]/g)].map(m => [m[1], m[2]]);
  same(Object.fromEntries(flags), manifest.cli.metadataFlags, 'METADATA_FLAGS');
  const secretNames = /const SECRET_NAMES = new Set\(\[([^\]]*)\]\)/.exec(cliSrc)?.[1].replace(/\s/g, '');
  check(secretNames === manifest.cli.secretArgument.secretNames.map(n => `'${n}'`).join(','), 'SECRET_NAMES drifted');
  check(new RegExp(`const SECRET_ARGUMENT_ERROR = '${manifest.cli.secretArgument.message.replace(/[.*+?^${}()|[\]\\;]/g, '\\$&')}'`).test(cliSrc), 'SECRET_ARGUMENT_ERROR drifted');
  const safeTypes = /const SAFE_CREDENTIAL_TYPES = new Set\(\[([^\]]*)\]\)/.exec(cliSrc)?.[1].replace(/\s/g, '');
  check(safeTypes === manifest.cli.secretArgument.allowedTypeValues.map(n => `'${n}'`).join(','), 'SAFE_CREDENTIAL_TYPES drifted');

  // Exit-code probes: child process, isolated HOME and registry path, stdin closed (non-TTY), no network.
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), 'hpm-compat-'));
  try {
    for (const probe of manifest.cli.exitCodeProbes) {
      const run = spawnSync(process.execPath, [path.join(root, 'src', 'cli.js'), ...probe.argv], {
        cwd: sandbox,
        input: '',
        encoding: 'utf8',
        timeout: 20000,
        env: { PATH: process.env.PATH, HOME: sandbox, USERPROFILE: sandbox, HAPPY_CONFIG_PATH: path.join(sandbox, 'instances.json'), ...probe.env }
      });
      const label = `probe ${JSON.stringify(probe.argv)}`;
      check(run.status === probe.exit, `${label}: exit ${run.status} != ${probe.exit}\n      stderr: ${truncate(run.stderr || '', 200)}`);
      const stream = probe.stream === 'stderr' ? run.stderr : run.stdout;
      check((stream || '').includes(probe.contains), `${label}: ${probe.stream} lacks ${JSON.stringify(probe.contains)}`);
    }
  } finally {
    fs.rmSync(sandbox, { recursive: true, force: true });
  }
}

async function checkStartup(manifest, root) {
  begin('startup');
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), 'hpm-compat-'));
  const cwd = process.cwd();
  const savedEnv = { ...process.env };
  try {
    process.chdir(sandbox); // dotenv.config() in stdio-server.js reads ./.env; keep the repo's .env out
    const { createConfiguredMcpServer } = await quiet(() => importFrom(root, 'src/stdio-server.js'));
    for (const row of manifest.startupModes.stdio.stdioMatrix) {
      let managerCalls = 0;
      const manager = {
        registry: {},
        getInstanceOrDefault() {
          managerCalls += 1;
          if (row.managerThrows) throw new Error(row.managerThrows);
          return { name: 'stub', url: 'https://stub.service-now.com', username: 'u', password: 'p' };
        }
      };
      let outcome;
      try {
        const started = await quiet(() => createConfiguredMcpServer({
          env: row.env,
          manager,
          credentialStore: {},
          ServiceNowClientClass: class { constructor() {} },
          createServer: async () => ({})
        }));
        outcome = started.docsOnly ? 'docs-only' : 'full';
      } catch {
        outcome = 'throws';
      }
      check(outcome === row.outcome, `stdio matrix "${row.case}": ${outcome} != ${row.outcome}`);
      check(managerCalls === row.managerCalls, `stdio matrix "${row.case}": config manager called ${managerCalls}x != ${row.managerCalls}`);
    }
  } finally {
    process.chdir(cwd);
    for (const key of Object.keys(process.env)) if (!(key in savedEnv)) delete process.env[key];
    Object.assign(process.env, savedEnv);
    fs.rmSync(sandbox, { recursive: true, force: true });
  }
}

async function checkConfig(manifest, root) {
  begin('config');
  const { resolveConfigPaths, USER_CONFIG_RELATIVE_PATH } = await importFrom(root, 'src/config-path.js');
  check(USER_CONFIG_RELATIVE_PATH === manifest.config.userPathRelative, `user config relative path ${USER_CONFIG_RELATIVE_PATH}`);
  for (const row of manifest.config.precedenceMatrix) {
    const existing = new Set(row.existing);
    const actual = resolveConfigPaths({
      env: row.env,
      homeDir: '/home/u',
      legacyPath: '/pkg/config/servicenow-instances.json',
      existsSync: p => existing.has(p)
    });
    same(actual, row.expect, `config precedence "${row.case}"`);
  }
}

async function checkKeychain(manifest, root) {
  begin('keychain');
  const clientSrc = readText(root, 'src/servicenow-client.js');
  const tupleMatch = /const tuple = \[([\s\S]*?)\];/.exec(clientSrc);
  const tupleText = tupleMatch ? tupleMatch[1].split('\n').map(l => l.trim()).filter(Boolean).join(' ') : '';
  check(tupleText === manifest.keychain.refreshToken.tupleSource, `identity tuple source drifted:\n      ${tupleText}`);
  check(clientSrc.includes(`createHash('sha256').update(JSON.stringify(tuple), 'utf8').digest('hex')`), 'identity hash expression drifted');
  check(clientSrc.includes(`const REFRESH_TOKEN_IDENTITY_PREFIX = '${manifest.keychain.refreshToken.accountPrefix}';`), 'identity account prefix drifted');
  const client = await importFrom(root, 'src/servicenow-client.js');
  for (const vector of manifest.keychain.refreshToken.testVectors) {
    check(client.oauthRefreshTokenAccount(vector.instance, { osUser: vector.osUser }) === vector.account, `identity-v1 vector "${vector.case}" drifted`);
    const tuple = ['happy-platform-mcp/oauth-refresh-token', 1, vector.osUser, vector.canonicalUrl, vector.instance.clientId, vector.authorizeHref, vector.tokenHref];
    check(vector.account === `identity-v1-${sha256(JSON.stringify(tuple))}`, `identity-v1 vector "${vector.case}" is not sha256(JSON.stringify(tuple)) of its declared tuple`);
  }
  check(client.legacyRefreshTokenAccount('dev', { osUser: 'alice' }) === manifest.keychain.refreshToken.legacyAccountExample, 'legacy refresh-token account format drifted');

  const credentials = await importFrom(root, 'src/instance-credential-store.js');
  for (const [type, expected] of Object.entries(manifest.keychain.instanceCredentials.examples)) {
    check(credentials.credentialRefFor('dev-1', type) === expected, `credentialRefFor(dev-1, ${type}) != ${expected}`);
  }
  const store = new credentials.InstanceCredentialStore();
  check(store.service === manifest.keychain.service, `credential store service ${store.service}`);
  const accounts = [];
  const probe = new credentials.InstanceCredentialStore({
    createEntry(service, account) {
      accounts.push([service, account]);
      let value = null;
      return { setPassword: async v => { value = v; }, getPassword: async () => value, deletePassword: async () => { value = null; return true; } };
    }
  });
  await probe.setSecret('keychain:instance/dev-1/password', 'x');
  await probe._verifyBackendHealth();
  check(accounts[0].join('|') === `${manifest.keychain.service}|keychain:instance/dev-1/password`, 'instance credential (service, account) tuple drifted');
  check(new RegExp(manifest.keychain.healthCheck.accountRegex).test(accounts[1][1]) && accounts[1][0] === manifest.keychain.service, `health-check account ${accounts[1][1]} does not match ${manifest.keychain.healthCheck.accountRegex}`);

  const tokens = await importFrom(root, 'src/token-store.js');
  const tokenAccounts = [];
  const ts = new tokens.KeychainTokenStore({
    createEntry(service, account) {
      tokenAccounts.push([service, account]);
      return { getPassword: async () => null, setPassword: async () => {}, deletePassword: async () => true };
    }
  });
  await ts.getRefreshToken('identity-v1-abc');
  check(tokenAccounts[0].join('|') === `${manifest.keychain.service}|identity-v1-abc`, 'refresh-token keychain (service, account) tuple drifted');
  check(tokens.isValidTokenAccount('identity-v1-abc') && !tokens.isValidTokenAccount('a/b') && !tokens.isValidTokenAccount('a..b'), 'token account grammar drifted');
  const accountRegex = /isValidTokenAccount\(account\) \{\s*return typeof account === 'string' && (\/[^\n]*?\/)\.test/.exec(readText(root, 'src/token-store.js'));
  check(accountRegex?.[1] === manifest.keychain.tokenAccountGrammar, `token account grammar regex ${accountRegex?.[1]}`);

  const savedXdg = process.env.XDG_CONFIG_HOME;
  try {
    process.env.XDG_CONFIG_HOME = '/xdg-test';
    check(new tokens.FileTokenStore().baseDir === '/xdg-test/happy-platform-mcp', 'file token dir with XDG_CONFIG_HOME drifted');
    delete process.env.XDG_CONFIG_HOME;
    check(new tokens.FileTokenStore().baseDir === path.join(os.homedir(), '.config', 'happy-platform-mcp'), 'file token dir default drifted');
  } finally {
    if (savedXdg === undefined) delete process.env.XDG_CONFIG_HOME; else process.env.XDG_CONFIG_HOME = savedXdg;
  }
  const fileStore = new tokens.FileTokenStore({ baseDir: '/tmp/hpm-compat-token-dir' });
  const account = manifest.keychain.refreshToken.testVectors[0].account;
  check(fileStore._fileFor(account) === `/tmp/hpm-compat-token-dir/token-${sha256(account)}`, 'file token name token-{sha256(account)} drifted');
  const fsSrc = readText(root, 'src/token-store.js');
  for (const needle of manifest.fileTokenStore.sourceSnippets) check(fsSrc.includes(needle), `token-store.js lost ${JSON.stringify(needle)}`);
  check(tokens.createDefaultTokenStore('') instanceof tokens.KeychainTokenStore && tokens.createDefaultTokenStore('file') instanceof tokens.FileTokenStore, 'SERVICENOW_TOKEN_STORE selection drifted');
  let invalid = null;
  try { tokens.createDefaultTokenStore('bogus'); } catch (error) { invalid = error.message; }
  check(invalid === 'SERVICENOW_TOKEN_STORE must be "keychain" or "file", got "bogus"', `invalid SERVICENOW_TOKEN_STORE message: ${invalid}`);
}

function checkSnippets(manifest, root) {
  begin('snippets');
  for (const entry of manifest.sourceSnippets) {
    let text;
    try { text = readText(root, entry.file); } catch { check(false, `${entry.file} missing`); continue; }
    for (const needle of entry.contains) check(text.includes(needle), `${entry.file} lost ${JSON.stringify(needle)} (${entry.note})`);
  }
}

// ---------------------------------------------------------------- main
async function main() {
  let opts;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(`check-manifest: ${error.message}`);
    return 2;
  }

  if (opts.emit) {
    const proto = await quiet(() => deriveProtocol(opts.root));
    const env = Object.fromEntries([...scanEnvVars(opts.root)].map(([name, sites]) => [name, sites]));
    const { tools, docsOnlyTools, ...rest } = proto;
    process.stdout.write(`${JSON.stringify({ tools: tools.map(toolDigest), docsOnly: docsOnlyTools.map(t => t.name), protocol: rest, env }, null, 2)}\n`);
    return 0;
  }

  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(opts.manifest, 'utf8'));
  } catch (error) {
    console.error(`check-manifest: cannot read manifest ${opts.manifest}: ${error.message}`);
    return 2;
  }

  begin('init');
  check(manifest.manifestVersion === 1, `unsupported manifestVersion ${manifest.manifestVersion}`);
  try {
    checkPackage(manifest, opts.root);
    const proto = await quiet(() => deriveProtocol(opts.root));
    checkTools(manifest, proto, opts);
    checkProtocol(manifest, proto);
    checkEnv(manifest, opts.root);
    checkCli(manifest, opts.root);
    await checkStartup(manifest, opts.root);
    await checkConfig(manifest, opts.root);
    await checkKeychain(manifest, opts.root);
    checkSnippets(manifest, opts.root);
    checkSources(manifest, opts.root, opts.strictSources);
  } catch (error) {
    failures.push(`${section}: checker crashed: ${error.stack || error.message}`);
  }

  for (const [name, counts] of sectionCounts) {
    if (name === 'init' && counts.failed === 0 && counts.checks === 1) continue;
    process.stdout.write(`${counts.failed ? 'FAIL' : 'ok  '} ${name} (${counts.checks} checks${counts.failed ? `, ${counts.failed} failed` : ''})\n`);
  }
  for (const warning of warnings) process.stdout.write(`WARN ${warning}\n`);
  if (failures.length) {
    process.stdout.write(`\n${failures.length} drift finding(s):\n`);
    for (const failure of failures) process.stdout.write(`  - ${failure}\n`);
    return 1;
  }
  process.stdout.write('\ncompat manifest matches the tree: no drift\n');
  return 0;
}

main().then(code => { process.exitCode = code; }, error => {
  console.error(`check-manifest: ${error.stack || error.message}`);
  process.exitCode = 2;
});
