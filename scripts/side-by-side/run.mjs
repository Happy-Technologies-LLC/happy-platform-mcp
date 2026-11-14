#!/usr/bin/env node
// Side-by-side harness: happy-platform-mcp 6.2.0 (Node) vs the Rust port, both
// over stdio against the PDI dev451274, same JSON-RPC sequence into both.
//
// Safety model (see README.md):
//  - Pinned to dev451274: the children get SERVICENOW_INSTANCE_URL/USERNAME/
//    PASSWORD from the environment in an ISOLATED HOME, so no instance registry
//    (and therefore no `ccfplg` entry) exists for them; the resolved host is
//    verified before any other call and the run aborts otherwise.
//  - The password is read once from the macOS keychain, passed only through the
//    child env, and every logged line is scrubbed; leaks are counted.
//  - Only the tools in READ_TOOLS / WRITE_TOOLS can be called; write tools may
//    only touch the `incident` table, and only records the calling server created
//    (prefix hpm-p5-sbs-<runid>-js|rs). No update-set, scope, property or script
//    tools are reachable.
//  - Everything created is deleted through the pinned REST helper and verified.
import { spawn, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadPassword, keychainAccounts } from './creds.mjs';
import { makeSn, HOST } from './sn.mjs';
import { Normalizer, diff } from './normalize.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '..', '..');
const PDI_URL = 'https://' + HOST;
const FORBIDDEN = 'ccfplg';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sha256 = (buf) => createHash('sha256').update(buf).digest('hex');

// ---------------------------------------------------------------- arguments
function parseArgs(argv) {
  const a = { out: '/tmp/hpm-p5-sbs', phase: 'all', jsMode: 'npx' };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    const v = () => argv[++i];
    if (k === '--out') a.out = v();
    else if (k === '--js-prefix') a.jsPrefix = v();
    else if (k === '--rust-bin') a.rustBin = v();
    else if (k === '--run-id') a.runId = v();
    else if (k === '--phase') a.phase = v(); // reads | all
    else if (k === '--js-mode') a.jsMode = v(); // npx | installed
    else throw new Error('unknown argument ' + k);
  }
  a.jsPrefix ??= path.join(a.out, 'js');
  a.rustBin ??= path.join(REPO, 'target/release/servicenow-mcp-server');
  a.runId ??= Date.now().toString(36);
  if (!/^[a-z0-9]{4,12}$/.test(a.runId)) throw new Error('run id must be [a-z0-9]{4,12}');
  if (!['reads', 'all'].includes(a.phase)) throw new Error('phase must be reads|all');
  return a;
}
const A = parseArgs(process.argv.slice(2));
const RUN_DIR = path.join(A.out, `run-${A.runId}`);
fs.mkdirSync(path.join(RUN_DIR, 'steps'), { recursive: true });
const PREFIX = { js: `hpm-p5-sbs-${A.runId}-js`, rs: `hpm-p5-sbs-${A.runId}-rs` };
const RUN_PREFIX_ALL = `hpm-p5-sbs-${A.runId}`;

// ------------------------------------------------------------ tool allowlist
const READ_TOOLS = new Set([
  'SN-Get-Current-Instance', 'SN-Query-Table', 'SN-Get-Record', 'SN-Get-Table-Schema',
  'SN-List-Incidents', 'SN-Get-Incident', 'SN-Get-Current-Update-Set', 'SN-Docs-Families',
  'SN-Explain-Field', 'SN-List-Available-Tables', 'SN-Discover-Table-Schema',
  'SN-List-SysUsers', 'SN-Natural-Language-Search',
]);
const WRITE_TOOLS = new Set([
  'SN-Create-Record', 'SN-Update-Record', 'SN-Create-Incident', 'SN-Add-Work-Notes',
  'SN-Add-Comment', 'SN-Resolve-Incident', 'SN-Batch-Create', 'SN-Batch-Update',
]);
const UNKNOWN_TOOL = 'SN-Does-Not-Exist';
const ZERO_ID = '0'.repeat(32);

// ----------------------------------------------------------- secrets / logs
const pw = loadPassword(A.jsPrefix);
const secretForms = [pw, Buffer.from('admin:' + pw).toString('base64'), encodeURIComponent(pw)]
  .filter((s, i, arr) => s.length >= 8 && arr.indexOf(s) === i);
let leaks = 0;
function scrub(s) {
  let out = s;
  for (const f of secretForms) {
    if (out.includes(f)) { leaks++; out = out.split(f).join('[REDACTED]'); }
  }
  return out;
}
const logFiles = new Map();
function log(file, obj) {
  const line = scrub(JSON.stringify({ t: new Date().toISOString(), ...obj })) + '\n';
  fs.appendFileSync(path.join(RUN_DIR, file), line, { mode: 0o600 });
  return line;
}
function say(msg) { console.log(scrub(msg)); }
void logFiles;

// -------------------------------------------------------------- JSON-RPC child
class Server {
  constructor(side, argv, env, cwd) {
    this.side = side;
    this.argv = argv;
    this.env = env;
    this.cwd = cwd;
    this.nextId = 1;
    this.pending = new Map();
    this.notes = [];
    this.stderrLines = [];
    this.buf = '';
    this.exited = null;
  }
  start() {
    this.child = spawn(this.argv[0], this.argv.slice(1), {
      cwd: this.cwd, env: this.env, stdio: ['pipe', 'pipe', 'pipe'], detached: true,
    });
    this.child.stdout.on('data', (d) => this.onStdout(scrub(d.toString())));
    this.child.stderr.on('data', (d) => {
      const s = scrub(d.toString());
      fs.appendFileSync(path.join(RUN_DIR, `stderr-${this.side}.log`), s, { mode: 0o600 });
      for (const l of s.split('\n')) if (l.trim()) this.stderrLines.push(l);
    });
    this.child.on('exit', (code, sig) => {
      this.exited = { code, sig };
      log(`transcript-${this.side}.jsonl`, { event: 'child-exit', code, sig });
      for (const [, p] of this.pending) p({ transportError: `child exited (code=${code}, signal=${sig})` });
      this.pending.clear();
    });
    this.child.stdin.on('error', () => {});
  }
  onStdout(s) {
    this.buf += s;
    let i;
    while ((i = this.buf.indexOf('\n')) >= 0) {
      const line = this.buf.slice(0, i);
      this.buf = this.buf.slice(i + 1);
      if (!line.trim()) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { log(`transcript-${this.side}.jsonl`, { event: 'unparsed-stdout', line }); continue; }
      if (msg.id != null && this.pending.has(msg.id) && (msg.result !== undefined || msg.error !== undefined)) {
        this.pending.get(msg.id)(msg);
        this.pending.delete(msg.id);
      } else {
        this.notes.push(msg);
        log(`transcript-${this.side}.jsonl`, { event: 'server-msg', msg });
      }
    }
  }
  notify(method, params) {
    const m = { jsonrpc: '2.0', method, ...(params ? { params } : {}) };
    log(`transcript-${this.side}.jsonl`, { dir: 'notify', method });
    this.child.stdin.write(JSON.stringify(m) + '\n');
  }
  /** Resolves with the JSON-RPC message or {transportError}. */
  rpc(method, params, timeoutMs = 120_000) {
    const id = this.nextId++;
    this.notes = [];
    const t0 = Date.now();
    log(`transcript-${this.side}.jsonl`, { dir: 'req', id, method, params });
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve({ transportError: `timeout after ${timeoutMs} ms` });
      }, timeoutMs);
      this.pending.set(id, (msg) => {
        clearTimeout(timer);
        msg.__ms = Date.now() - t0;
        msg.__notes = this.notes;
        log(`transcript-${this.side}.jsonl`, { dir: 'res', id, method, ms: msg.__ms, res: msg });
        resolve(msg);
      });
      this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
  }
  async stop() {
    if (!this.child || this.exited) return;
    try { this.child.stdin.end(); } catch { /* closed */ }
    for (let i = 0; i < 30 && !this.exited; i++) await sleep(100);
    if (!this.exited) { try { process.kill(-this.child.pid, 'SIGTERM'); } catch { /* gone */ } }
    for (let i = 0; i < 30 && !this.exited; i++) await sleep(100);
    if (!this.exited) { try { process.kill(-this.child.pid, 'SIGKILL'); } catch { /* gone */ } }
  }
}

// --------------------------------------------------------------- helpers
function textOf(resp) {
  const items = resp?.result?.content;
  return Array.isArray(items) ? items.filter((c) => c.type === 'text').map((c) => c.text).join('\n') : '';
}
const HEX32 = /\b[0-9a-f]{32}\b/g;
const INC = /\bINC\d{7,}\b/g;
const uniq = (arr) => [...new Set(arr)];
function canonical(v) {
  if (Array.isArray(v)) return v.map(canonical);
  if (v && typeof v === 'object') return Object.fromEntries(Object.keys(v).sort().map((k) => [k, canonical(v[k])]));
  return v;
}
const eq = (a, b) => JSON.stringify(canonical(a)) === JSON.stringify(canonical(b));

// --------------------------------------------------------------- state
const N = new Normalizer(A.runId);
const steps = []; // summary rows
const created = { js: [], rs: [] }; // [{label, sys_id, number?}] per side, in order
const meta = { runId: A.runId, started: new Date().toISOString(), phase: A.phase };
const servers = {};

function createdId(side, label) {
  const r = created[side].find((c) => c.label === label && c.sys_id);
  if (!r) throw new Error(`no created record ${label} for ${side}`);
  return r.sys_id;
}
function createdNumber(side, label) {
  const r = created[side].find((c) => c.label === label && c.number);
  if (!r) throw new Error(`no created number ${label} for ${side}`);
  return r.number;
}

/** Register side-specific ids/numbers found in a create-class response. */
function registerCreated(label, resps) {
  const hex = { js: uniq(textOf(resps.js).match(HEX32) ?? []), rs: uniq(textOf(resps.rs).match(HEX32) ?? []) };
  const num = { js: uniq(textOf(resps.js).match(INC) ?? []), rs: uniq(textOf(resps.rs).match(INC) ?? []) };
  for (const side of ['js', 'rs']) {
    const other = side === 'js' ? 'rs' : 'js';
    const own = hex[side].filter((h) => !hex[other].includes(h));
    own.forEach((h, i) => {
      N.register(side, h, `<ID:${label}.${i + 1}>`);
      created[side].push({ label: `${label}.${i + 1}`, sys_id: h });
    });
    const ownN = num[side].filter((h) => !num[other].includes(h));
    ownN.forEach((h, i) => {
      N.register(side, h, `<NUM:${label}.${i + 1}>`);
      created[side].push({ label: `${label}.${i + 1}`, number: h });
    });
  }
}

function guardCall(side, name, args) {
  if (name === UNKNOWN_TOOL) return; // error-path probe: no such tool exists on either server
  if (!READ_TOOLS.has(name) && !WRITE_TOOLS.has(name)) throw new Error(`tool not allowed: ${name}`);
  if (args && args.instance !== undefined) throw new Error('harness never passes `instance`');
  if (!WRITE_TOOLS.has(name)) return;
  const own = new Set(created[side].map((c) => c.sys_id).filter(Boolean));
  const okId = (id) => id === ZERO_ID || own.has(id);
  const tables = [];
  if (args.table_name) tables.push(args.table_name);
  for (const o of args.operations ?? []) tables.push(o.table);
  for (const u of args.updates ?? []) { tables.push(u.table); if (!okId(u.sys_id)) throw new Error(`write to a record this server did not create: ${u.sys_id}`); }
  if (args.sys_id !== undefined && !okId(args.sys_id)) throw new Error(`write to a record this server did not create: ${args.sys_id}`);
  if (name === 'SN-Create-Incident' || name === 'SN-Add-Work-Notes' || name === 'SN-Add-Comment' || name === 'SN-Resolve-Incident') tables.push('incident');
  // The one non-incident name is a table that does not exist (error-path probe), so nothing can be written to it.
  if (tables.length === 0 || tables.some((t) => t !== 'incident' && t !== 'hpm_no_such_table_zz')) throw new Error(`write tools may only touch incident, got ${JSON.stringify(tables)}`);
  const text = JSON.stringify(args);
  if (name !== 'SN-Update-Record' || args.sys_id !== ZERO_ID) {
    if (!text.includes(PREFIX[side])) throw new Error(`write payload lacks the ${PREFIX[side]} prefix`);
  }
  if (args.incident_number && !created[side].some((c) => c.number === args.incident_number)) {
    throw new Error(`incident ${args.incident_number} was not created by ${side}`);
  }
}

const TRANSIENT = /^Error: (500|502|503)\b/;

/** Run one request on both servers; returns {js, rs} raw responses. */
async function callBoth(method, paramsFor, { write = false } = {}) {
  const out = {};
  const retries = { js: 0, rs: 0 };
  for (const side of ['js', 'rs']) {
    const params = typeof paramsFor === 'function' ? paramsFor(side) : paramsFor;
    if (method === 'tools/call') guardCall(side, params.name, params.arguments);
    let r;
    for (let attempt = 0; ; attempt++) {
      r = await servers[side].rpc(method, params);
      const transient = !write && r.result?.isError && TRANSIENT.test(textOf(r));
      if (!transient || attempt >= 3) break;
      retries[side]++;
      await sleep(3000 * (attempt + 1));
    }
    out[side] = r;
  }
  out.retries = retries;
  return out;
}

/**
 * Record one compared step.
 * opts: kind, ts (normalise run timestamps), register (label: ids/numbers created),
 *       expected: [{path: RegExp, cite}], expectedCheck(rawJs, rawRs, normJs, normRs) -> cite|null
 */
function record(id, title, resps, opts = {}) {
  if (opts.register) registerCreated(opts.register, resps);
  const view = (side) => {
    const r = resps[side];
    if (r.transportError) return { transportError: r.transportError };
    const o = r.error ? { error: r.error } : { result: r.result };
    if (opts.notifications !== false) o.notifications = (r.__notes ?? []).map((n) => ({ method: n.method, params: n.params }));
    return o;
  };
  const nj = N.response('js', view('js'), { ts: opts.ts });
  const nr = N.response('rs', view('rs'), { ts: opts.ts });
  const d = diff(nj, nr);
  let cls = 'match';
  let cites = [];
  let unexplained = [];
  if (d.length) {
    const whole = opts.expectedCheck?.(resps.js, resps.rs, nj, nr);
    if (whole) { cls = 'expected-diff'; cites = [whole]; } else {
      const rules = opts.expected ?? [];
      unexplained = d.filter((x) => !rules.some((e) => e.path.test(x.path)));
      cites = uniq(d.map((x) => rules.find((e) => e.path.test(x.path))?.cite).filter(Boolean));
      cls = unexplained.length ? 'unexpected-diff' : 'expected-diff';
    }
  }
  const row = {
    id, title, kind: opts.kind ?? 'read', class: cls, diffs: d.length, unexplained: unexplained.length,
    cites, retries: resps.retries, ms: { js: resps.js.__ms, rs: resps.rs.__ms },
    isError: { js: !!resps.js.result?.isError || !!resps.js.error, rs: !!resps.rs.result?.isError || !!resps.rs.error },
  };
  steps.push(row);
  fs.writeFileSync(path.join(RUN_DIR, 'steps', `${id}.json`), scrub(JSON.stringify({ ...row, diffList: d, unexplainedList: unexplained, js: nj, rs: nr }, null, 2)) + '\n', { mode: 0o600 });
  say(`${cls.padEnd(15)} ${id}  ${title}${d.length ? `  (${d.length} diff lines)` : ''}`);
  return row;
}

async function tool(id, title, name, args, opts = {}) {
  const write = WRITE_TOOLS.has(name);
  const resps = await callBoth('tools/call', (side) => ({ name, arguments: typeof args === 'function' ? args(side) : args }), { write });
  return record(id, title, resps, { ...opts, kind: write ? 'write' : 'read', ts: opts.ts ?? write });
}

// ------------------------------------------------------------ before / after
function registryPath() { return path.join(os.homedir(), '.config/happy-platform-mcp/instances.json'); }
function fileSha(p) { try { return sha256(fs.readFileSync(p)); } catch (e) { return 'unreadable:' + e.code; } }
const sn = makeSn(pw);

async function snapshot(label, startUtc) {
  const s = {
    label, at: new Date().toISOString(),
    registrySha256: fileSha(registryPath()),
    keychainAccounts: keychainAccounts(A.jsPrefix),
    sysUpdateXmlTotal: await sn.count('sys_update_xml'),
    sysUpdateXmlSinceStart: startUtc ? await sn.count('sys_update_xml', `sys_created_on>=${startUtc}`) : null,
    incidentTotal: await sn.count('incident'),
    incidentsWithAnySbsPrefix: await sn.count('incident', 'short_descriptionSTARTSWITHhpm-p5-sbs-'),
    incidentsWithRunPrefix: await sn.count('incident', `short_descriptionSTARTSWITH${RUN_PREFIX_ALL}`),
    updateSetPref: (await sn.rows('sys_user_preference', 'name=sys_update_set^user.user_name=admin', 'value,sys_updated_on', 5)).map((r) => r.value),
  };
  fs.writeFileSync(path.join(RUN_DIR, `snapshot-${label}.json`), JSON.stringify(s, null, 2) + '\n', { mode: 0o600 });
  return s;
}

// ------------------------------------------------------------------- main
async function main() {
  const abort = async (why) => {
    say('ABORT: ' + why);
    for (const s of Object.values(servers)) await s.stop();
    meta.aborted = why;
    fs.writeFileSync(path.join(RUN_DIR, 'results.json'), JSON.stringify({ meta, steps, leaks }, null, 2) + '\n');
    process.exit(2);
  };

  // 0. binaries and versions
  if (!fs.existsSync(A.rustBin)) throw new Error('Rust binary not found: ' + A.rustBin);
  meta.rust = {
    bin: A.rustBin, sha256: sha256(fs.readFileSync(A.rustBin)),
    gitSha: execFileSync('git', ['-C', REPO, 'rev-parse', 'HEAD']).toString().trim(),
    gitDirty: execFileSync('git', ['-C', REPO, 'status', '--porcelain', '--untracked-files=no']).toString().trim() !== '',
  };
  const jsPkgPath = path.join(A.jsPrefix, 'node_modules/happy-platform-mcp/package.json');
  if (!fs.existsSync(jsPkgPath)) {
    say('installing happy-platform-mcp@6.2.0 into ' + A.jsPrefix);
    fs.mkdirSync(A.jsPrefix, { recursive: true });
    execFileSync('npm', ['install', '--prefix', A.jsPrefix, 'happy-platform-mcp@6.2.0', '--ignore-scripts', '--no-audit', '--no-fund'], { stdio: 'inherit' });
  }
  meta.js = { version: JSON.parse(fs.readFileSync(jsPkgPath, 'utf8')).version, mode: A.jsMode };
  if (meta.js.version !== '6.2.0') throw new Error('expected happy-platform-mcp 6.2.0, found ' + meta.js.version);

  // 1. PDI reachable and awake? (stop, do not try anything else, if not)
  const probe = await sn.get('/api/now/table/sys_user?sysparm_limit=1&sysparm_fields=user_name&sysparm_query=user_name=admin');
  if (probe.status !== 200 || !probe.json?.result) {
    await abort(`PDI ${HOST} unreachable or hibernating (HTTP ${probe.status}, ${probe.text.slice(0, 80).replace(/\s+/g, ' ')})`);
  }
  const startUtc = new Date(Date.now() - 120_000).toISOString().replace('T', ' ').slice(0, 19);
  meta.startUtc = startUtc;
  const before = await snapshot('before', startUtc);
  if (before.incidentsWithAnySbsPrefix !== 0) await abort('stale hpm-p5-sbs-* incidents exist before the run; clean them first');
  const adminId = (await sn.rows('sys_user', 'user_name=admin', 'sys_id', 1))[0]?.sys_id;
  const oldestInc = (await sn.rows('incident', 'ORDERBYnumber^numberSTARTSWITHINC', 'sys_id,number', 1))[0];
  if (!adminId || !oldestInc) await abort('could not resolve fixture sys_ids');
  meta.fixtures = { adminSysId: adminId, oldestIncident: oldestInc };

  // 2. start both servers, identical env (HOME/cwd differ only by side)
  const env = (side) => {
    const home = path.join(RUN_DIR, 'home', side);
    fs.mkdirSync(path.join(home, 'tmp'), { recursive: true });
    fs.mkdirSync(path.join(RUN_DIR, 'work', side), { recursive: true });
    return {
      PATH: `${path.dirname(process.execPath)}:/usr/bin:/bin:/usr/sbin:/sbin`,
      HOME: home,
      XDG_CONFIG_HOME: path.join(home, '.config'),
      TMPDIR: path.join(home, 'tmp'),
      npm_config_update_notifier: 'false',
      SERVICENOW_INSTANCE_URL: PDI_URL,
      SERVICENOW_USERNAME: 'admin',
      SERVICENOW_PASSWORD: pw,
    };
  };
  const jsArgv = A.jsMode === 'installed'
    ? [path.join(A.jsPrefix, 'node_modules/.bin/servicenow-mcp-server')]
    : [path.join(path.dirname(process.execPath), 'npx'), '-y', '-p', 'happy-platform-mcp@6.2.0', 'servicenow-mcp-server'];
  meta.commands = { js: jsArgv.join(' '), rs: A.rustBin };
  servers.js = new Server('js', jsArgv, env('js'), path.join(RUN_DIR, 'work/js'));
  servers.rs = new Server('rs', [A.rustBin], env('rs'), path.join(RUN_DIR, 'work/rs'));
  // identical env key set and values except the per-side directories
  const ej = env('js'), er = env('rs');
  meta.envKeys = Object.keys(ej).sort();
  meta.envIdentical = Object.keys(ej).every((k) => ['HOME', 'XDG_CONFIG_HOME', 'TMPDIR'].includes(k) || ej[k] === er[k]) && eq(Object.keys(ej).sort(), Object.keys(er).sort());
  servers.js.start();
  servers.rs.start();

  // 3. initialize
  const init = {};
  for (const side of ['js', 'rs']) {
    init[side] = await servers[side].rpc('initialize', {
      protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'hpm-p5-side-by-side', version: '1' },
    }, 240_000);
    if (!init[side].result) await abort(`${side} initialize failed: ${JSON.stringify(init[side]).slice(0, 200)}`);
    servers[side].notify('notifications/initialized');
  }
  meta.serverInfo = { js: init.js.result.serverInfo, rs: init.rs.result.serverInfo };
  await sleep(1500); // let startup banners land on stderr

  // 4. identity: resolved host must be dev451274 before anything else
  const idResps = await callBoth('tools/call', { name: 'SN-Get-Current-Instance', arguments: {} });
  for (const side of ['js', 'rs']) {
    const t = textOf(idResps[side]);
    if (!t.includes(HOST) || t.toLowerCase().includes(FORBIDDEN)) await abort(`${side} resolved instance is not ${HOST}: ${t.slice(0, 200)}`);
  }
  const resList = await callBoth('resources/list', {});
  for (const side of ['js', 'rs']) {
    for (const r of resList[side].result?.resources ?? []) {
      if (JSON.stringify(r).toLowerCase().includes(FORBIDDEN)) await abort(`${side} resources mention ${FORBIDDEN}`);
    }
  }
  meta.identityVerified = true;
  say(`identity verified: both servers resolve ${HOST}`);

  // ---------------------------------------------------------- READ SET
  record('R00-initialize', 'initialize (serverInfo, protocolVersion, capabilities)', { js: init.js, rs: init.rs, retries: { js: 0, rs: 0 } }, {
    notifications: false,
    expected: [
      { path: /^result\.serverInfo\.version$/, cite: 'versions differ by definition (6.2.0 vs 7.0.x)' },
      { path: /^result\.capabilities\.prompts$/, cite: 'p4/conformance.md gap 1 + C9 (SEP-2575): 7.0 declares tools + resources only; 6.2.0 declared an empty `prompts` it never served' },
    ],
  });
  await toolsList();
  record('R02-resources-list', 'resources/list', resList, {});
  const uris = (resList.js.result?.resources ?? []).map((r) => r.uri);
  for (const uri of uris) {
    const resp = await callBoth('resources/read', { uri });
    record(`R03-resource-${uri.replace(/[^a-z0-9]+/gi, '_')}`, `resources/read ${uri}`, resp, {
      expected: [{ path: /server_info\.version$/, cite: 'versions differ by definition (6.2.0 vs 7.0.x)' }],
    });
  }
  record('R04-resource-unknown', 'resources/read servicenow://nope (unknown URI)', await callBoth('resources/read', { uri: 'servicenow://nope' }), {
    expectedCheck: (rj, rr) => (rj.error?.code === -32603 && rr.error?.code === -32602 && rr.error?.data?.uri === 'servicenow://nope')
      ? 'notes-pending Lane D / owner decision SEP-2164: unknown resource URI is -32602 + data.uri in 7.0 (6.2.0: -32603)' : null,
  });
  record('R05-prompts-list', 'prompts/list (client on protocol 2025-06-18)', await callBoth('prompts/list', {}), {
    expectedCheck: (rj, rr) => (rj.error?.code === -32601 && Array.isArray(rr.result?.prompts) && rr.result.prompts.length === 0)
      ? 'crates/hpm-tools/tests/golden_surface.rs DEVIATIONS prompts/list (empty list; 6.2.0: -32601) and 7.0-migration-notes.md (prompts/list: older clients get an empty list). Note p4/conformance.md gap 1 wording "legacy keeps the 6.2.0 empty list" is imprecise: live 6.2.0 answers -32601' : null,
  });
  record('R06-ping', 'ping', await callBoth('ping', {}), {});
  record('R07-unknown-method', 'bogus/method (unknown JSON-RPC method)', await callBoth('bogus/method', {}), {
    expectedCheck: (rj, rr) => (rj.error?.code === -32601 && rj.error?.message === 'Method not found' && rr.error?.code === -32601 && rr.error?.message === 'bogus/method')
      ? 'crates/hpm-tools/tests/golden_surface.rs DEVIATIONS `no/such/method`: -32601 message names the method (6.2.0: `Method not found`)' : null,
  });
  record('R10-current-instance', 'SN-Get-Current-Instance', idResps, {});

  await tool('R11-query-sys_user', 'SN-Query-Table sys_user admin (fixed fields/limit)', 'SN-Query-Table',
    { table_name: 'sys_user', query: 'user_name=admin', fields: 'user_name,first_name,last_name,active,sys_id', limit: 1 });
  await tool('R12-query-sys_db_object', 'SN-Query-Table sys_db_object ordered', 'SN-Query-Table',
    { table_name: 'sys_db_object', query: 'nameINincident,problem,change_request,task', fields: 'name,label', order_by: 'name', limit: 10 });
  await tool('R13-query-sys_dictionary', 'SN-Query-Table sys_dictionary incident.state', 'SN-Query-Table',
    { table_name: 'sys_dictionary', query: 'name=incident^elementINstate,priority,number', fields: 'name,element,internal_type,max_length', order_by: 'element', limit: 5 });
  await tool('R14-query-incident-oldest', 'SN-Query-Table incident oldest by number', 'SN-Query-Table',
    { table_name: 'incident', query: 'numberSTARTSWITHINC', fields: 'number,short_description,state,priority,category', order_by: 'number', limit: 3 });
  await tool('R15-query-offset', 'SN-Query-Table with offset', 'SN-Query-Table',
    { table_name: 'incident', query: 'numberSTARTSWITHINC', fields: 'number', order_by: 'number', limit: 2, offset: 2 });
  await tool('R16-get-record-user', 'SN-Get-Record sys_user admin, fields', 'SN-Get-Record',
    { table_name: 'sys_user', sys_id: adminId, fields: 'user_name,first_name,last_name,active' });
  await tool('R17-get-record-incident', 'SN-Get-Record oldest incident (all fields)', 'SN-Get-Record',
    { table_name: 'incident', sys_id: oldestInc.sys_id });
  await tool('R18-table-schema-incident', 'SN-Get-Table-Schema incident', 'SN-Get-Table-Schema', { table_name: 'incident' });
  await tool('R19-table-schema-user', 'SN-Get-Table-Schema sys_user', 'SN-Get-Table-Schema', { table_name: 'sys_user' });
  await tool('R20-list-incidents', 'SN-List-Incidents oldest 3', 'SN-List-Incidents',
    { query: 'numberSTARTSWITHINC', fields: 'number,short_description,priority', order_by: 'number', limit: 3 });
  await tool('R21-list-incidents-priority', 'SN-List-Incidents priority filter', 'SN-List-Incidents',
    { priority: 1, fields: 'number,priority', order_by: 'number', limit: 3 });
  await tool('R22-get-incident', 'SN-Get-Incident oldest', 'SN-Get-Incident', { sys_id: oldestInc.sys_id });
  await tool('R23-current-update-set', 'SN-Get-Current-Update-Set', 'SN-Get-Current-Update-Set', {});
  await tool('R24-docs-families', 'SN-Docs-Families', 'SN-Docs-Families', {});
  await tool('R25-explain-field', 'SN-Explain-Field incident.state', 'SN-Explain-Field', { table: 'incident', field: 'state' });
  await tool('R26-explain-field-examples-off', 'SN-Explain-Field incident.priority, no examples', 'SN-Explain-Field',
    { table: 'incident', field: 'priority', include_examples: false });
  await tool('R27-list-available-tables', 'SN-List-Available-Tables', 'SN-List-Available-Tables', {});
  await tool('R28-discover-table-schema', 'SN-Discover-Table-Schema sys_user_group', 'SN-Discover-Table-Schema', { table_name: 'sys_user_group' });
  await tool('R29-list-sysusers', 'SN-List-SysUsers admin', 'SN-List-SysUsers',
    { query: 'user_name=admin', fields: 'user_name,active', limit: 1 });
  await tool('R30-natural-language', 'SN-Natural-Language-Search incidents', 'SN-Natural-Language-Search',
    { query: 'incidents', fields: 'number', order_by: 'number', limit: 2 });
  // error paths (read-only, nothing is written)
  await tool('E01-get-record-missing', 'SN-Get-Record all-zero sys_id (404)', 'SN-Get-Record', { table_name: 'incident', sys_id: ZERO_ID });
  await tool('E02-query-missing-table', 'SN-Query-Table nonexistent table', 'SN-Query-Table', { table_name: 'hpm_no_such_table_zz', limit: 1 });
  await tool('E03-query-bad-table-name', 'SN-Query-Table table_name "bad table!" (TF-1)', 'SN-Query-Table', { table_name: 'bad table!', limit: 1 }, {
    expectedCheck: (rj, rr) => (rr.error?.code === -32602 && /table_name/.test(rr.error?.message ?? ''))
      ? 'deviations.rs SN-Query-Table/invalid-table-arg (TF-1): 7.0 rejects with -32602 naming table_name before any request' : null,
  });
  await tool('E04-schema-missing-table', 'SN-Get-Table-Schema nonexistent table', 'SN-Get-Table-Schema', { table_name: 'hpm_no_such_table_zz' });
  await tool('E05-explain-missing-field', 'SN-Explain-Field nonexistent field', 'SN-Explain-Field', { table: 'incident', field: 'hpm_no_such_field' });
  await tool('E06-missing-required', 'SN-Get-Record missing required sys_id', 'SN-Get-Record', { table_name: 'incident' }, {
    expectedCheck: (rj, rr) => (rj.result?.isError && rr.error?.code === -32602 && rr.error?.message === 'missing required property: sys_id')
      ? 'crates/hpm-tools/tests/golden_surface.rs DEVIATIONS surface/full-default step 15: 7.0 validates arguments against the tool schema before any request (6.2.0 sent the request with sys_id undefined)' : null,
  });
  record('E07-unknown-tool', 'tools/call unknown tool', await callBoth('tools/call', { name: UNKNOWN_TOOL, arguments: {} }), {
    expectedCheck: (rj, rr) => (rj.result?.isError && textOf(rj) === `Error: Unknown tool: ${UNKNOWN_TOOL}` && rr.error?.code === -32602 && rr.error?.message === `Unknown tool: ${UNKNOWN_TOOL}`)
      ? 'crates/hpm-tools/tests/golden_surface.rs DEVIATIONS SN-Does-Not-Exist (RFC section 10.1 step 1, C6): unknown tool is -32602, not an isError result' : null,
  });

  if (A.phase === 'all') await writeSet();

  // ------------------------------------------------------- stderr banners
  stderrCompare();

  for (const s of Object.values(servers)) await s.stop();
  await finish(before, startUtc);
}

// -------------------------------------------------------------- tools/list
async function toolsList() {
  const resps = await callBoth('tools/list', {});
  const fx62 = JSON.parse(fs.readFileSync(path.join(REPO, 'fixtures/tools-list-6.2.0.json'), 'utf8')).tools;
  const fx70 = JSON.parse(fs.readFileSync(path.join(REPO, 'fixtures/tools-list-7.0.json'), 'utf8')).tools;
  const byName = (arr) => new Map(arr.map((t) => [t.name, t]));
  const J = resps.js.result?.tools ?? [], R = resps.rs.result?.tools ?? [];
  const mj = byName(J), mr = byName(R), m62 = byName(fx62), m70 = byName(fx70);
  const detail = { counts: { js: J.length, rs: R.length }, sameNamesAndOrder: eq(J.map((t) => t.name), R.map((t) => t.name)), perTool: [] };
  let match = 0, expected = 0, unexpected = 0;
  for (const name of uniq([...mj.keys(), ...mr.keys()])) {
    const a = mj.get(name), b = mr.get(name);
    if (!a || !b) { unexpected++; detail.perTool.push({ name, class: 'unexpected-diff', note: 'present on one side only' }); continue; }
    if (eq(a, b)) { match++; continue; }
    const fields = uniq([...Object.keys(a), ...Object.keys(b)]).filter((k) => !eq(a[k], b[k]));
    const jsIsFx62 = eq(a, m62.get(name)), rsIsFx70 = eq(b, m70.get(name));
    const cls = jsIsFx62 && rsIsFx70 ? 'expected-diff' : 'unexpected-diff';
    if (cls === 'expected-diff') expected++; else unexpected++;
    detail.perTool.push({ name, class: cls, fields, jsEqualsFixture62: jsIsFx62, rsEqualsFixture70: rsIsFx70 });
  }
  const orderUnexpected = !detail.sameNamesAndOrder;
  // every differing field must be one of the recorded 7.0 changes
  const fieldsSeen = uniq(detail.perTool.flatMap((t) => t.fields ?? []));
  detail.fieldsDiffering = fieldsSeen;
  detail.cite = 'fixtures/tools-list-6.2.0.json -> tools-list-7.0.json: D-05 `annotations` on all 60 tools (Q1), SN-Batch-Create `transaction` description (owner decision 11), SN-Execute-Background-Script allow_trigger_fallback (P4-2); p3/exit-gate-audit.md item 2';
  const cls = unexpected || orderUnexpected ? 'unexpected-diff' : expected ? 'expected-diff' : 'match';
  steps.push({
    id: 'R01-tools-list', title: `tools/list (${J.length} vs ${R.length} tools: ${match} identical, ${expected} expected-diff, ${unexpected} unexpected)`,
    kind: 'read', class: cls, diffs: expected + unexpected, unexpected, cites: expected ? [detail.cite] : [], ms: { js: resps.js.__ms, rs: resps.rs.__ms },
    isError: { js: !!resps.js.error, rs: !!resps.rs.error },
  });
  fs.writeFileSync(path.join(RUN_DIR, 'steps', 'R01-tools-list.json'), JSON.stringify(detail, null, 2) + '\n', { mode: 0o600 });
  say(`${cls.padEnd(15)} R01-tools-list  ${J.length} vs ${R.length}: ${match} identical, ${expected} expected, ${unexpected} unexpected; fields ${fieldsSeen.join(',')}`);
}

// --------------------------------------------------------------- write set
async function writeSet() {
  say('--- write set: each server creates its own incident records');
  const P = (side) => PREFIX[side];
  // Reads of the records this run created: their timestamps differ by seconds between the two servers.
  const wtool = (id, title, name, args, opts = {}) => tool(id, title, name, args, { ts: true, ...opts });
  await wtool('W01-create-record', 'SN-Create-Record incident', 'SN-Create-Record',
    (s) => ({ table_name: 'incident', data: { short_description: `${P(s)} create-record`, urgency: '2', impact: '2', category: 'inquiry' } }),
    { register: 'W01' });
  await wtool('W02-get-record', 'SN-Get-Record created incident', 'SN-Get-Record',
    (s) => ({ table_name: 'incident', sys_id: createdId(s, 'W01.1') }), { ts: true });
  await wtool('W03-update-record', 'SN-Update-Record description+urgency', 'SN-Update-Record',
    (s) => ({ table_name: 'incident', sys_id: createdId(s, 'W01.1'), data: { description: `${P(s)} updated`, urgency: '3' } }));
  await wtool('W04-get-record-after-update', 'SN-Get-Record after update (fields)', 'SN-Get-Record',
    (s) => ({ table_name: 'incident', sys_id: createdId(s, 'W01.1'), fields: 'short_description,description,urgency,priority,impact,category,state' }));
  await wtool('W05-query-own-prefix', 'SN-Query-Table own prefix', 'SN-Query-Table',
    (s) => ({ table_name: 'incident', query: `short_descriptionSTARTSWITH${P(s)}^ORDERBYshort_description`, fields: 'short_description,description,urgency', limit: 10 }));
  await wtool('W06-create-incident', 'SN-Create-Incident', 'SN-Create-Incident',
    (s) => ({ short_description: `${P(s)} create-incident`, description: `${P(s)} description`, impact: '3', urgency: '3' }), { register: 'W06' });
  await wtool('W07-get-incident', 'SN-Get-Incident created', 'SN-Get-Incident', (s) => ({ sys_id: createdId(s, 'W06.1') }));
  await wtool('W08-work-notes', 'SN-Add-Work-Notes', 'SN-Add-Work-Notes',
    (s) => ({ incident_number: createdNumber(s, 'W06.1'), work_notes: `${P(s)} work note` }));
  await wtool('W09-comment', 'SN-Add-Comment', 'SN-Add-Comment',
    (s) => ({ incident_number: createdNumber(s, 'W06.1'), comment: `${P(s)} comment` }));
  await wtool('W10-batch-create', 'SN-Batch-Create 2 incidents', 'SN-Batch-Create',
    (s) => ({ operations: [
      { table: 'incident', data: { short_description: `${P(s)} batch-a` }, save_as: 'a' },
      { table: 'incident', data: { short_description: `${P(s)} batch-b` } },
    ] }), { register: 'W10' });
  await wtool('W11-batch-update', 'SN-Batch-Update the 2 batch incidents', 'SN-Batch-Update',
    (s) => ({ updates: [
      { table: 'incident', sys_id: createdId(s, 'W10.1'), data: { description: `${P(s)} batch update a` } },
      { table: 'incident', sys_id: createdId(s, 'W10.2'), data: { description: `${P(s)} batch update b` } },
    ] }));
  await wtool('W12-update-missing', 'SN-Update-Record all-zero sys_id (error shape)', 'SN-Update-Record',
    (s) => ({ table_name: 'incident', sys_id: ZERO_ID, data: { description: `${P(s)} nothing` } }));
  await wtool('W13-batch-create-failure', 'SN-Batch-Create op0 ok, op1 invalid table (transaction)', 'SN-Batch-Create',
    (s) => ({ operations: [
      { table: 'incident', data: { short_description: `${P(s)} batch-fail-ok` } },
      { table: 'hpm_no_such_table_zz', data: { short_description: `${P(s)} batch-fail-bad` } },
    ] }), {
    register: 'W13',
    expectedCheck: (rj, rr) => {
      const a = textOf(rj), b = textOf(rr);
      return (rj.result?.isError && rr.result?.isError && b.startsWith(a) && /\nAlready created and not rolled back: \{.*operation_0.*\}\. Do not re-run the whole batch\.$/s.test(b.slice(a.length)))
        ? 'deviations.rs SN-Batch-Create/failure-rollback: 7.0 names the records it did not roll back and says not to re-run the batch (owner rule)' : null;
    },
  });
  await wtool('W14-resolve', 'SN-Resolve-Incident', 'SN-Resolve-Incident',
    (s) => ({ incident_number: createdNumber(s, 'W06.1'), resolution_code: 'Solution provided', resolution_notes: `${P(s)} resolved` }));
  await wtool('W15-get-incident-resolved', 'SN-Get-Incident after resolve', 'SN-Get-Incident', (s) => ({ sys_id: createdId(s, 'W06.1') }), { ts: true });
  await wtool('W16-query-all-own', 'SN-Query-Table all own-prefix records', 'SN-Query-Table',
    (s) => ({ table_name: 'incident', query: `short_descriptionSTARTSWITH${P(s)}^ORDERBYshort_description`, fields: 'short_description,state,urgency', limit: 50 }));

  // Independent view of what each server actually wrote (REST, not the tool under test).
  await restCompare();
}

const REC_FIELDS = 'short_description,description,urgency,impact,priority,state,category,caller_id,assigned_to,assignment_group,sys_class_name,sys_created_by,sys_mod_count,resolved_by,close_code,close_notes,opened_by,contact_type,incident_state';
async function restCompare() {
  const rows = {};
  for (const side of ['js', 'rs']) {
    const r = await sn.rows('incident', `short_descriptionSTARTSWITH${PREFIX[side]}^ORDERBYshort_description`, REC_FIELDS, 50);
    rows[side] = r.map((x) => Object.fromEntries(Object.entries(x).sort().map(([k, v]) => [k, typeof v === 'string' ? v.split(PREFIX[side]).join('<PFX>') : v])));
  }
  const d = diff(rows.js, rows.rs);
  const row = {
    id: 'W17-rest-records', title: `independent REST view of created records (js ${rows.js.length}, rs ${rows.rs.length})`, kind: 'write',
    class: d.length ? 'unexpected-diff' : 'match', diffs: d.length, unexplained: d.length, cites: [], ms: {}, isError: {},
  };
  steps.push(row);
  fs.writeFileSync(path.join(RUN_DIR, 'steps', 'W17-rest-records.json'), JSON.stringify({ ...row, diffList: d, js: rows.js, rs: rows.rs }, null, 2) + '\n', { mode: 0o600 });
  say(`${row.class.padEnd(15)} W17-rest-records  ${row.title}${d.length ? `  (${d.length} diff lines)` : ''}`);
}

// --------------------------------------------------------------- stderr
function stderrCompare() {
  const nl = (side) => servers[side].stderrLines.map((l) => N.str(side, l.replace(/call_id: CallId\("[^"]+"\)/g, 'call_id: <VOL>'), { ts: true }).replace(/^\d{4}-\d\d-\d\dT\S+\s+/, ''));
  const a = nl('js'), b = nl('rs');
  const onlyJs = a.filter((l) => !b.includes(l)), onlyRs = b.filter((l) => !a.includes(l));
  const detail = { js: a, rs: b, onlyJs, onlyRs };
  fs.writeFileSync(path.join(RUN_DIR, 'steps', 'S01-stderr.json'), JSON.stringify(detail, null, 2) + '\n', { mode: 0o600 });
  steps.push({
    id: 'S01-stderr', title: `stderr diagnostics (${onlyJs.length} lines only in 6.2.0, ${onlyRs.length} only in Rust; not protocol)`,
    kind: 'info', class: onlyJs.length || onlyRs.length ? 'info-diff' : 'match', diffs: onlyJs.length + onlyRs.length, cites: [], ms: {}, isError: {},
  });
  say(`info            S01-stderr  only-6.2.0=${onlyJs.length} only-rust=${onlyRs.length}`);
}

// ----------------------------------------------------------------- cleanup
async function finish(before, startUtc) {
  say('--- cleanup');
  const cleanup = { deleted: [], failures: [] };
  const rows = await sn.rows('incident', `short_descriptionSTARTSWITH${RUN_PREFIX_ALL}`, 'sys_id,number,short_description', 200);
  const ids = uniq([...rows.map((r) => r.sys_id), ...created.js.concat(created.rs).map((c) => c.sys_id).filter(Boolean)]);
  cleanup.foundByPrefix = rows.length;
  cleanup.byPrefixSide = { js: rows.filter((r) => r.short_description.startsWith(PREFIX.js)).length, rs: rows.filter((r) => r.short_description.startsWith(PREFIX.rs)).length };
  // journal rows (work notes / comments) before deleting, to see whether they cascade
  const journalBefore = ids.length ? await sn.count('sys_journal_field', `element_idIN${ids.join(',')}`) : 0;
  for (const id of ids) {
    const r = await sn.del('incident', id);
    if (r.status === 204 || r.status === 404) cleanup.deleted.push({ sys_id: id, http: r.status });
    else cleanup.failures.push({ sys_id: id, http: r.status, body: r.text.slice(0, 100) });
  }
  await sleep(2000);
  cleanup.journalRowsBefore = journalBefore;
  cleanup.journalRowsAfter = ids.length ? await sn.count('sys_journal_field', `element_idIN${ids.join(',')}`) : 0;
  const after = await snapshot('after', startUtc);
  cleanup.runPrefixIncidentsAfter = after.incidentsWithRunPrefix;
  cleanup.anySbsPrefixAfter = after.incidentsWithAnySbsPrefix;
  const uxml = await sn.rows('sys_update_xml', `sys_created_on>=${startUtc}`, 'name,target_name,sys_created_by,sys_created_on,update_set.name', 50);
  cleanup.sysUpdateXmlSinceStartRows = uxml;
  meta.cleanup = cleanup;
  meta.before = before;
  meta.after = after;
  meta.checks = {
    registryUnchanged: before.registrySha256 === after.registrySha256,
    keychainAccountsUnchanged: eq(before.keychainAccounts, after.keychainAccounts),
    prefixCountZero: after.incidentsWithRunPrefix === 0 && after.incidentsWithAnySbsPrefix === 0,
    incidentTotalEqual: before.incidentTotal === after.incidentTotal,
    sysUpdateXmlTotalEqual: before.sysUpdateXmlTotal === after.sysUpdateXmlTotal,
    sysUpdateXmlSinceStartZero: uxml.length === 0,
    updateSetPrefUnchanged: eq(before.updateSetPref, after.updateSetPref),
    noDeleteFailures: cleanup.failures.length === 0,
    envIdentical: meta.envIdentical,
    leaksZero: leaks === 0,
  };
  meta.finished = new Date().toISOString();
  const tally = steps.reduce((t, s) => ({ ...t, [s.class]: (t[s.class] ?? 0) + 1 }), {});
  meta.tally = tally;
  fs.writeFileSync(path.join(RUN_DIR, 'results.json'), scrub(JSON.stringify({ meta, leaks, steps }, null, 2)) + '\n', { mode: 0o600 });
  fs.writeFileSync(path.join(RUN_DIR, 'table.md'), table());
  say(`tally ${JSON.stringify(tally)}; leaks=${leaks}; checks ${JSON.stringify(meta.checks)}`);
  say('evidence: ' + RUN_DIR);
  if (leaks !== 0 || !Object.values(meta.checks).every(Boolean)) process.exitCode = 3;
}

function table() {
  const rows = ['| # | Step | Kind | Result | Diff lines | Citation / note |', '|---|------|------|--------|-----------:|-----------------|'];
  for (const s of steps) {
    const note = s.cites?.join('; ').replace(/\|/g, '\\|') ?? '';
    rows.push(`| ${s.id} | ${s.title.replace(/\|/g, '\\|')} | ${s.kind} | ${s.class} | ${s.diffs ?? 0} | ${note} |`);
  }
  return rows.join('\n') + '\n';
}

main().catch(async (e) => {
  console.error(scrub('HARNESS ERROR: ' + (e.stack || e)));
  for (const s of Object.values(servers)) await s.stop();
  // Best-effort cleanup of anything this run created, then fail.
  try {
    const rows = await sn.rows('incident', `short_descriptionSTARTSWITH${RUN_PREFIX_ALL}`, 'sys_id', 200);
    for (const r of rows) await sn.del('incident', r.sys_id);
    console.error(`emergency cleanup removed ${rows.length} record(s)`);
  } catch (e2) { console.error(scrub('emergency cleanup failed: ' + e2.message)); }
  process.exit(1);
});
