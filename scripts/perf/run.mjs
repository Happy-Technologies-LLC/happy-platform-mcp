#!/usr/bin/env node
/**
 * scripts/perf/run.mjs
 *
 * Hermetic perf sanity benchmark: Rust 7.0 vs Node 6.2.0, same machine,
 * nothing reaches the network or ServiceNow. Both servers are pointed at the
 * loopback stub (scripts/golden/lib/stub-sn.mjs). A temp HOME and temp
 * HAPPY_CONFIG_PATH keep both servers off the real registry/keychain; env
 * credentials are used. The HTTP bearer token is random, kept in memory,
 * never printed.
 *
 * Usage: node scripts/perf/run.mjs
 *
 * Prints a markdown table and writes /tmp/hpm-specs/p4/perf.md.
 *
 * v3 (verified):
 *   - `notifications/initialized` is sent as a real JSON-RPC notification
 *     (no id). Error counts should be 0/0; any remaining error is reported.
 *   - HTTP runs in two variants: (a) new TCP connection per request,
 *     (b) a shared http.Agent({keepAlive:true, maxSockets:8}).
 *   - Legacy SSE transport (GET /mcp + POST /mcp?sessionId=<id>) for both
 *     servers.
 *   - Streamable HTTP (POST /mcp with Mcp-Session-Id header, 2025-11-25
 *     session mode and 2026-07-28 stateless) for Rust 7.0 only — Node 6.2.0
 *     does not support Streamable HTTP (answers 400).
 *   - The 8x concurrent phase reports distinct session ids per server.
 */

import { startStub } from '../golden/lib/stub-sn.mjs';
import { statSync, mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { execSync, spawn } from 'node:child_process';
import { join, sep } from 'node:path';
import http from 'node:http';
import crypto from 'node:crypto';

const REPO = '/Users/nczitzer/WebstormProjects/hpm-w4-e';
const RUST_STDIO = REPO + '/target/release/servicenow-mcp-server';
const RUST_HTTP = REPO + '/target/release/happy-platform-mcp-http';
const NODE_STDIO = REPO + '/src/stdio-server.js';
const NODE_HTTP = REPO + '/src/server.js';

const FAKE_USER = 'golden.user';
const FAKE_PASSWORD = 'golden-fake-password-4f9a1c';

const TRIALS_COLD = 10;
const LIST_CALLS = 200;
const QUERY_CALLS = 500;
const CONCURRENT_CLIENTS = 8;
const RSS_CALLS = 1000;

function nowNs() { return process.hrtime.bigint(); }
function nsToMs(n) { return Number(n) / 1_000_000; }

function percentile(sorted, p) {
  if (sorted.length === 0) return 0;
  const idx = Math.ceil((p / 100) * sorted.length) - 1;
  return sorted[Math.max(0, Math.min(idx, sorted.length - 1))];
}
function median(values) {
  const s = [...values].sort((a, b) => a - b);
  if (s.length % 2 === 1) return s[Math.floor(s.length / 2)];
  return (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
}

function rssKb(pid) {
  const out = execSync('ps -o rss= -p ' + pid, { encoding: 'utf8' }).trim();
  return Number(out) || 0;
}
function fileSize(path) {
  try { return statSync(path).size; } catch { return 0; }
}
function spawnCmd(cmd) {
  return cmd.endsWith('.js') ? ['node', cmd] : [cmd];
}

// True when a JSON-RPC response is not an error.
function isJsonRpcSuccess(msg) {
  if (!msg || typeof msg !== 'object') return false;
  if (msg.error) return false;
  if (msg.result && msg.result.isError === true) return false;
  return true;
}

// Parse a response body into the JSON-RPC message, or null. Tries plain JSON
// first, then SSE `data: ` lines (trying each, not just the first).
function parseResponseBody(text) {
  try { return JSON.parse(text); } catch { }
  const lines = text.split('\n');
  for (const line of lines) {
    if (!line.startsWith('data: ')) continue;
    const payload = line.slice(6);
    if (!payload.trim()) continue;
    try { return JSON.parse(payload); } catch { continue; }
  }
  return null;
}

// ---------------------------------------------------------------------------
// Stub
// ---------------------------------------------------------------------------

let stub;
async function startStubServer() {
  stub = await startStub({ host: '127.0.0.1' });
  return 'http://127.0.0.1:' + stub.port;
}
async function stopStubServer() { if (stub) await stub.close(); }

// ---------------------------------------------------------------------------
// Server process management
// ---------------------------------------------------------------------------

function makeSandbox() {
  const root = mkdtempSync(join(tmpdir(), 'hpm-perf-'));
  const home = join(root, 'home');
  const cfgDir = join(root, 'cfg');
  mkdirRecursive(home);
  mkdirRecursive(cfgDir);
  return { root, home, configPath: join(cfgDir, 'instances.json') };
}
function mkdirRecursive(p) {
  const parts = p.split(sep);
  let acc = sep;
  for (const part of parts) {
    if (!part) continue;
    acc = join(acc, part);
    try { mkdirSync(acc, { recursive: true }); } catch { }
  }
}

function baseEnv(home, configPath, stubUrl, extra) {
  if (extra === void 0) extra = {};
  return {
    PATH: process.env.PATH,
    HOME: home,
    USERPROFILE: home,
    TMPDIR: join(home, 'tmp'),
    TZ: 'UTC',
    LANG: 'C.UTF-8',
    NO_COLOR: '1',
    HAPPY_CONFIG_PATH: configPath,
    SERVICENOW_INSTANCE_URL: stubUrl,
    SERVICENOW_USERNAME: FAKE_USER,
    SERVICENOW_PASSWORD: FAKE_PASSWORD,
    ...extra,
  };
}

function spawnServer(cmd, args, env) {
  return spawn(cmd, args, { env, stdio: ['pipe', 'pipe', 'pipe'] });
}
function killServer(child) {
  try { child.kill('SIGKILL'); } catch { }
  try { child.stdin?.end?.(); } catch { }
  try { child.stdout?.destroy?.(); } catch { }
  try { child.stderr?.destroy?.(); } catch { }
}

// ---------------------------------------------------------------------------
// MCP stdio session
// ---------------------------------------------------------------------------

class StdioSession {
  constructor(child) {
    this.child = child;
    this.buf = '';
    this.id = 0;
    this.pending = new Map();
    this.lines = [];
    this.errors = 0;
    this.sessionId = null;
    child.stdout.on('data', d => { this.buf += d.toString(); this._drain(); });
  }

  _drain() {
    let idx;
    while ((idx = this.buf.indexOf('\n')) !== -1) {
      this.lines.push(this.buf.slice(0, idx));
      this.buf = this.buf.slice(idx + 1);
    }
    for (let i = 0; i < this.lines.length; i++) {
      const line = this.lines[i];
      if (!line.trim()) { this.lines.splice(i, 1); i--; continue; }
      let msg;
      try { msg = JSON.parse(line); } catch { this.lines.splice(i, 1); i--; continue; }
      if (msg.jsonrpc === '2.0' && msg.id !== undefined) {
        const cb = this.pending.get(msg.id);
        if (cb) { this.pending.delete(msg.id); cb(msg); }
      }
      this.lines.splice(i, 1);
      i--;
    }
  }

  async request(method, params, timeoutMs) {
    if (params === void 0) params = {};
    if (timeoutMs === void 0) timeoutMs = 10000;
    const id = ++this.id;
    const msg = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n';
    this.child.stdin.write(msg);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error('timeout waiting for ' + id + ':' + method));
      }, timeoutMs);
      this.pending.set(id, (msg) => {
        clearTimeout(timer);
        if (!isJsonRpcSuccess(msg)) this.errors++;
        resolve(msg);
      });
    });
  }

  async notify(method, params) {
    if (params === void 0) params = {};
    const msg = JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n';
    this.child.stdin.write(msg);
  }

  async init() {
    await this.request('initialize', {
      protocolVersion: '2025-11-25',
      capabilities: {},
      clientInfo: { name: 'perf-bench', version: '1' },
    });
    await this.notify('notifications/initialized', {});
  }

  async toolsList() { return this.request('tools/list'); }
  async toolsCall(name, args) { return this.request('tools/call', { name, arguments: args }); }
  close() { killServer(this.child); }
}

async function spawnStdio(cmd, stubUrl, extraEnv) {
  const sandbox = makeSandbox();
  const env = baseEnv(sandbox.home, sandbox.configPath, stubUrl, extraEnv);
  const sc = spawnCmd(cmd);
  const child = spawnServer(sc[0], sc[1] ? [sc[1]] : [], env);
  const session = new StdioSession(child);
  return { session, child, pid: child.pid, sandbox, stop: () => killServer(child) };
}
async function initStdio(s) { await s.session.init(); }
async function startStdio(cmd, stubUrl, extraEnv) {
  const s = await spawnStdio(cmd, stubUrl, extraEnv);
  await initStdio(s);
  return s;
}
async function coldStartStdio(cmd, stubUrl, extraEnv) {
  const trials = [];
  let totalErrors = 0;
  for (let i = 0; i < TRIALS_COLD; i++) {
    const s = await spawnStdio(cmd, stubUrl, extraEnv);
    const t0 = nowNs();
    await initStdio(s);
    await s.session.toolsList();
    const t1 = nowNs();
    totalErrors += s.session.errors;
    s.stop();
    trials.push(nsToMs(t1 - t0));
  }
  return { ms: median(trials), errors: totalErrors };
}

// stdio concurrent: one process per client, all inited before timing.
async function stdioConcurrentSessions(cmd, stubUrl) {
  const sessions = [];
  for (let i = 0; i < CONCURRENT_CLIENTS; i++) {
    const s = await spawnStdio(cmd, stubUrl, {});
    await initStdio(s);
    sessions.push(s.session);
  }
  return { sessions, stopAll: () => sessions.forEach(s => s.close()) };
}

// ---------------------------------------------------------------------------
// MCP HTTP session (legacy SSE: GET /mcp + POST /mcp?sessionId=<id>)
// ---------------------------------------------------------------------------

class SseSession {
  constructor(baseUrl, token, agent) {
    this.baseUrl = baseUrl;
    this.token = token;
    this.sessionId = null;
    this.id = 0;
    this.errors = 0;
    this.agent = agent;
    this.sseBuf = '';
    this.pending = new Map();
    this._sseReader = null;
  }

  async openSseSession() {
    const url = new URL('/mcp', this.baseUrl);
    const res = await fetch(url, {
      headers: {
        'Authorization': 'Bearer ' + this.token,
        'Accept': 'text/event-stream',
      },
    });
    if (!res.ok) throw new Error('SSE session failed: ' + res.status);
    this._sseReader = res.body.getReader();
    const dec = new TextDecoder();
    (async () => {
      let buf = '';
      try {
        while (true) {
          const { value, done } = await this._sseReader.read();
          if (done) break;
          buf += dec.decode(value, { stream: true });
          this.sseBuf = buf;
          let idx;
          while ((idx = buf.indexOf('\n\n')) !== -1) {
            const block = buf.slice(0, idx);
            buf = buf.slice(idx + 2);
            const dataLine = block.split('\n').find(l => l.startsWith('data: '));
            if (!dataLine) continue;
            let msg;
            try { msg = JSON.parse(dataLine.slice(6)); } catch { continue; }
            if (msg.jsonrpc === '2.0' && msg.id !== undefined) {
              const cb = this.pending.get(msg.id);
              if (cb) { this.pending.delete(msg.id); cb(msg); }
            }
          }
        }
      } catch { }
    })();

    await new Promise((resolve, reject) => {
      const poll = () => {
        const m = this.sseBuf.match(/data:\s*(\/mcp\?sessionId=[^\s\n]+)/);
        if (m) {
          this.sessionId = new URL(m[1], 'http://x').searchParams.get('sessionId');
          if (this.sessionId) return resolve();
        }
        if (!this._sseReader) return reject(new Error('SSE reader not started'));
        setTimeout(poll, 20);
      };
      setTimeout(poll, 20);
    });
    if (!this.sessionId) throw new Error('SSE session id not found');
  }

  async rpc(method, params) {
    if (params === void 0) params = {};
    if (!this.sessionId) await this.openSseSession();
    const id = ++this.id;
    const body = JSON.stringify({ jsonrpc: '2.0', id, method, params });
    const path = '/mcp?sessionId=' + encodeURIComponent(this.sessionId);
    let rpcResult = null;
    const rpcReady = new Promise((resolve, reject) => {
      this.pending.set(id, (msg) => { rpcResult = msg; resolve(msg); });
      setTimeout(() => {
        if (this.pending.has(id)) { this.pending.delete(id); reject(new Error('timeout waiting for ' + id + ':' + method)); }
      }, 10000);
    });
    const res = await fetch(new URL(path, this.baseUrl), {
      method: 'POST',
      headers: {
        'Authorization': 'Bearer ' + this.token,
        'Content-Type': 'application/json',
        'Accept': 'application/json, text/event-stream',
      },
      body,
    });
    await rpcReady;
    let responseError = false;
    if (res.status < 200 || res.status >= 300) {
      if (!(method.startsWith('notifications/') && res.status === 202)) {
        responseError = true;
      }
    }
    if (rpcResult && (rpcResult.error || (rpcResult.result && rpcResult.result.isError))) {
      responseError = true;
    }
    if (responseError) this.errors++;
    return { status: res.status, parsed: rpcResult };
  }

  async notify(method, params) {
    if (params === void 0) params = {};
    if (!this.sessionId) await this.openSseSession();
    const body = JSON.stringify({ jsonrpc: '2.0', method, params });
    const path = '/mcp?sessionId=' + encodeURIComponent(this.sessionId);
    const res = await fetch(new URL(path, this.baseUrl), {
      method: 'POST',
      headers: {
        'Authorization': 'Bearer ' + this.token,
        'Content-Type': 'application/json',
        'Accept': 'application/json, text/event-stream',
      },
      body,
    });
    let responseError = false;
    if (res.status < 200 || res.status >= 300) {
      if (!(method.startsWith('notifications/') && res.status === 202)) {
        responseError = true;
      }
    }
    if (responseError) this.errors++;
    return { status: res.status };
  }

  async init() {
    await this.rpc('initialize', {
      protocolVersion: '2025-11-25',
      capabilities: {},
      clientInfo: { name: 'perf-bench', version: '1' },
    });
    await this.notify('notifications/initialized', {});
  }
  async toolsList() { return this.rpc('tools/list'); }
  async toolsCall(name, args) { return this.rpc('tools/call', { name, arguments: args }); }
  close() { }
}

// ---------------------------------------------------------------------------
// MCP HTTP session (Streamable HTTP: POST /mcp with Mcp-Session-Id header)
// ---------------------------------------------------------------------------

class StreamableSession {
  constructor(baseUrl, token, agent, mode) {
    this.baseUrl = baseUrl;
    this.token = token;
    this.sessionId = null;
    this.id = 0;
    this.errors = 0;
    this.agent = agent;
    this.mode = mode;
  }

  async rpc(method, params) {
    if (params === void 0) params = {};
    const isNotify = method.startsWith('notifications/');
    const id = isNotify ? null : ++this.id;
    const headers = {
      'Authorization': 'Bearer ' + this.token,
      'Content-Type': 'application/json',
      'Accept': 'application/json, text/event-stream',
    };
    let body;
    if (this.mode === 'session') {
      headers['MCP-Protocol-Version'] = '2025-11-25';
      if (this.sessionId) headers['Mcp-Session-Id'] = this.sessionId;
      body = JSON.stringify({ jsonrpc: '2.0', ...(id ? { id } : {}), method, params });
    } else {
      headers['MCP-Protocol-Version'] = '2026-07-28';
      headers['Mcp-Method'] = method;
      if (method === 'tools/call' && typeof params === 'object' && params !== null && params.name) {
        headers['Mcp-Name'] = params.name;
      }
      if (isNotify) {
        body = JSON.stringify({ jsonrpc: '2.0', method, params });
      } else {
        const p = typeof params === 'object' && params !== null ? { ...params } : {};
        p._meta = {
          'io.modelcontextprotocol/protocolVersion': '2026-07-28',
          'io.modelcontextprotocol/clientInfo': { name: 'perf-bench', version: '1' },
          'io.modelcontextprotocol/clientCapabilities': {},
        };
        body = JSON.stringify({ jsonrpc: '2.0', id, method, params: p });
      }
    }
    const res = await fetch(new URL('/mcp', this.baseUrl), {
      method: 'POST',
      headers,
      body,
    });

    if (isNotify) {
      let responseError = false;
      if (res.status < 200 || res.status >= 300) responseError = true;
      if (responseError) this.errors++;
      return { status: res.status, parsed: null };
    }

    const text = await res.text();
    let parsed = parseResponseBody(text);
    if (parsed === null) {
      if (!(res.status === 202 && !text.trim())) {
        parsed = { error: { message: 'unparseable: ' + text.slice(0, 200) } };
      }
    }

    if (this.mode === 'session') {
      const sid = res.headers.get('mcp-session-id');
      if (sid && !this.sessionId) this.sessionId = sid;
    }

    let responseError = false;
    if (res.status < 200 || res.status >= 300) responseError = true;
    if (parsed && (parsed.error || (parsed.result && parsed.result.isError))) responseError = true;
    if (responseError) this.errors++;
    return { status: res.status, parsed };
  }

  async notify(method, params) {
    if (params === void 0) params = {};
    return this.rpc(method, params);
  }

  async init() {
    if (this.mode === 'session') {
      await this.rpc('initialize', {
        protocolVersion: '2025-11-25',
        capabilities: {},
        clientInfo: { name: 'perf-bench', version: '1' },
      });
    } else {
      await this.rpc('initialize', {
        protocolVersion: '2026-07-28',
        capabilities: {},
        clientInfo: { name: 'perf-bench', version: '1' },
      });
    }
    await this.notify('notifications/initialized', {});
  }
  async toolsList() { return this.rpc('tools/list'); }
  async toolsCall(name, args) { return this.rpc('tools/call', { name, arguments: args }); }
  close() { }
}

async function findFreePort() {
  return new Promise((res, rej) => {
    const srv = http.createServer();
    srv.listen(0, '127.0.0.1', () => { const p = srv.address().port; srv.close(); res(p); });
    srv.on('error', rej);
  });
}

async function spawnHttpServer(cmd, stubUrl, port, extraEnv) {
  const sandbox = makeSandbox();
  const token = crypto.randomBytes(32).toString('hex');
  const env = baseEnv(sandbox.home, sandbox.configPath, stubUrl, {
    PORT: String(port),
    HAPPY_MCP_API_TOKEN: token,
    HAPPY_MCP_ALLOWED_HOSTS: '127.0.0.1:' + port,
    HAPPY_MCP_ALLOWED_ORIGINS: 'http://127.0.0.1:' + port,
    ...extraEnv,
  });
  const sc = spawnCmd(cmd);
  const child = spawnServer(sc[0], sc[1] ? [sc[1]] : [], env);
  const baseUrl = 'http://127.0.0.1:' + port;
  return { child, pid: child.pid, sandbox, token, baseUrl, stop: () => killServer(child) };
}

async function createSseSession(baseUrl, token, agent) {
  const session = new SseSession(baseUrl, token, agent);
  let ready = false;
  for (let i = 0; i < 100; i++) {
    try {
      await session.init();
      ready = true;
      break;
    } catch { await new Promise(r => setTimeout(r, 100)); }
  }
  if (!ready) throw new Error('SSE session did not become ready on ' + baseUrl);
  return { session };
}

async function createStreamableSession(baseUrl, token, agent, mode) {
  const session = new StreamableSession(baseUrl, token, agent, mode);
  let ready = false;
  for (let i = 0; i < 100; i++) {
    try {
      await session.init();
      ready = true;
      break;
    } catch { await new Promise(r => setTimeout(r, 100)); }
  }
  if (!ready) throw new Error('Streamable session did not become ready on ' + baseUrl);
  return { session };
}

async function startHttp(cmd, stubUrl, port, extraEnv, agent, sessionType) {
  if (sessionType === void 0) sessionType = 'sse';
  const server = await spawnHttpServer(cmd, stubUrl, port, extraEnv);
  if (sessionType === 'sse') {
    const { session } = await createSseSession(server.baseUrl, server.token, agent);
    return { session, child: server.child, pid: server.pid, token: server.token, baseUrl: server.baseUrl, stop: () => server.stop() };
  } else {
    const { session } = await createStreamableSession(server.baseUrl, server.token, agent, sessionType);
    return { session, child: server.child, pid: server.pid, token: server.token, baseUrl: server.baseUrl, stop: () => server.stop() };
  }
}

async function coldStartHttp(cmd, stubUrl, port, extraEnv, agent, sessionType) {
  if (sessionType === void 0) sessionType = 'sse';
  const trials = [];
  let totalErrors = 0;
  for (let i = 0; i < TRIALS_COLD; i++) {
    const server = await spawnHttpServer(cmd, stubUrl, port, extraEnv);
    const t0 = nowNs();
    if (sessionType === 'sse') {
      const { session } = await createSseSession(server.baseUrl, server.token, agent);
      await session.toolsList();
      totalErrors += session.errors;
    } else {
      const { session } = await createStreamableSession(server.baseUrl, server.token, agent, sessionType);
      await session.toolsList();
      totalErrors += session.errors;
    }
    const t1 = nowNs();
    server.stop();
    trials.push(nsToMs(t1 - t0));
  }
  return { ms: median(trials), errors: totalErrors };
}

async function httpConcurrentSessions(cmd, stubUrl, port, agent, sessionType) {
  if (sessionType === void 0) sessionType = 'sse';
  const server = await spawnHttpServer(cmd, stubUrl, port, {});
  const sessions = [];
  for (let i = 0; i < CONCURRENT_CLIENTS; i++) {
    if (sessionType === 'sse') {
      const { session } = await createSseSession(server.baseUrl, server.token, agent);
      sessions.push(session);
    } else {
      const { session } = await createStreamableSession(server.baseUrl, server.token, agent, sessionType);
      sessions.push(session);
    }
  }
  return { sessions, stopAll: () => server.stop() };
}

// ---------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------

async function rssMeasure(makeServer, callFn) {
  const s = await makeServer();
  const idle = rssKb(s.pid);
  for (let i = 0; i < RSS_CALLS; i++) {
    await callFn(s.session);
  }
  const after = rssKb(s.pid);
  const errors = s.session.errors;
  s.stop();
  return { idle, after, errors };
}

async function listLatency(makeServer) {
  const s = await makeServer();
  const times = [];
  for (let i = 0; i < LIST_CALLS; i++) {
    const t0 = nowNs();
    await s.session.toolsList();
    times.push(nsToMs(nowNs() - t0));
  }
  const errors = s.session.errors;
  s.stop();
  times.sort((a, b) => a - b);
  return { p50: percentile(times, 50), p99: percentile(times, 99), errors };
}

async function queryLatency(makeServer) {
  const s = await makeServer();
  const times = [];
  for (let i = 0; i < QUERY_CALLS; i++) {
    const t0 = nowNs();
    await s.session.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 });
    times.push(nsToMs(nowNs() - t0));
  }
  const errors = s.session.errors;
  s.stop();
  times.sort((a, b) => a - b);
  return { p50: percentile(times, 50), p99: percentile(times, 99), errors };
}

async function concurrentMeasure({ sessions, stopAll }) {
  const sessionIds = sessions.map(s => s.sessionId || null).filter(Boolean);
  const distinctIds = new Set(sessionIds).size;
  const start = nowNs();
  const times = [];
  const perClient = Math.ceil(QUERY_CALLS / CONCURRENT_CLIENTS);
  await Promise.all(sessions.map(s => runClient(s, perClient, times)));
  const durMs = nsToMs(nowNs() - start);
  const throughput = QUERY_CALLS / (durMs / 1000);
  const errors = sessions.reduce((sum, s) => sum + s.errors, 0);
  times.sort((a, b) => a - b);
  stopAll();
  return { throughput, p99: percentile(times, 99), errors, distinctIds, sessionIds };

  async function runClient(session, n, times) {
    for (let i = 0; i < n; i++) {
      const t0 = nowNs();
      await session.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 });
      times.push(nsToMs(nowNs() - t0));
    }
  }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  const stubUrl = await startStubServer();
  const port = await findFreePort();

  const rows = [];
  const infoLines = [];
  const na = 'n/a';

  function flagWorse(rustVal, nodeVal) {
    if (rustVal === 0 || nodeVal === 0) return '';
    return rustVal > nodeVal * 2 ? '\u26a0\ufe0f >2x' : '';
  }
  function flagBetterIsMore(rustVal, nodeVal) {
    if (rustVal === 0 || nodeVal === 0) return '';
    return rustVal < nodeVal / 2 ? '\u26a0\ufe0f >2x' : '';
  }
  function errCell(rErr, nErr) { return rErr + '/' + nErr; }

  // -------- stdio --------
  {
    const r = await coldStartStdio(RUST_STDIO, stubUrl, {});
    const n = await coldStartStdio(NODE_STDIO, stubUrl, {});
    rows.push(['stdio cold start (ms, median)', r.ms.toFixed(1), n.ms.toFixed(1), errCell(r.errors, n.errors), flagWorse(r.ms, n.ms)]);
  }
  {
    const r = await rssMeasure(() => startStdio(RUST_STDIO, stubUrl, {}), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    const n = await rssMeasure(() => startStdio(NODE_STDIO, stubUrl, {}), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    rows.push(['stdio idle RSS (KB)', r.idle.toFixed(0), n.idle.toFixed(0), errCell(r.errors, n.errors), flagWorse(r.idle, n.idle)]);
    rows.push(['stdio RSS after 1000 calls (KB)', r.after.toFixed(0), n.after.toFixed(0), errCell(r.errors, n.errors), flagWorse(r.after, n.after)]);
  }
  {
    const r = await listLatency(() => startStdio(RUST_STDIO, stubUrl, {}));
    const n = await listLatency(() => startStdio(NODE_STDIO, stubUrl, {}));
    rows.push(['stdio tools/list p50 (ms)', r.p50.toFixed(2), n.p50.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p50, n.p50)]);
    rows.push(['stdio tools/list p99 (ms)', r.p99.toFixed(2), n.p99.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p99, n.p99)]);
  }
  {
    const r = await queryLatency(() => startStdio(RUST_STDIO, stubUrl, {}));
    const n = await queryLatency(() => startStdio(NODE_STDIO, stubUrl, {}));
    rows.push(['stdio SN-Query-Table p50 (ms)', r.p50.toFixed(2), n.p50.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p50, n.p50)]);
    rows.push(['stdio SN-Query-Table p99 (ms)', r.p99.toFixed(2), n.p99.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p99, n.p99)]);
  }
  {
    const r = await concurrentMeasure(await stdioConcurrentSessions(RUST_STDIO, stubUrl));
    const n = await concurrentMeasure(await stdioConcurrentSessions(NODE_STDIO, stubUrl));
    rows.push(['stdio 8x conc throughput (calls/s)', r.throughput.toFixed(1), n.throughput.toFixed(1), errCell(r.errors, n.errors), flagBetterIsMore(r.throughput, n.throughput)]);
    rows.push(['stdio 8x conc p99 (ms)', r.p99.toFixed(2), n.p99.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p99, n.p99)]);
    infoLines.push('stdio 8x conc: distinct session ids N/A (stdio)');
  }

  // -------- http variant (a): new TCP per request, legacy SSE --------
  const agentA = null;
  {
    const r = await coldStartHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'sse');
    const n = await coldStartHttp(NODE_HTTP, stubUrl, port, {}, agentA, 'sse');
    rows.push(['http SSE cold start (ms, median) [a: new TCP/req]', r.ms.toFixed(1), n.ms.toFixed(1), errCell(r.errors, n.errors), flagWorse(r.ms, n.ms)]);
  }
  {
    const r = await rssMeasure(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'sse'), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    const n = await rssMeasure(() => startHttp(NODE_HTTP, stubUrl, port, {}, agentA, 'sse'), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    rows.push(['http SSE idle RSS (KB) [a]', r.idle.toFixed(0), n.idle.toFixed(0), errCell(r.errors, n.errors), flagWorse(r.idle, n.idle)]);
    rows.push(['http SSE RSS after 1000 calls (KB) [a]', r.after.toFixed(0), n.after.toFixed(0), errCell(r.errors, n.errors), flagWorse(r.after, n.after)]);
  }
  {
    const r = await listLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'sse'));
    const n = await listLatency(() => startHttp(NODE_HTTP, stubUrl, port, {}, agentA, 'sse'));
    rows.push(['http SSE tools/list p50 (ms) [a]', r.p50.toFixed(2), n.p50.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p50, n.p50)]);
    rows.push(['http SSE tools/list p99 (ms) [a]', r.p99.toFixed(2), n.p99.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p99, n.p99)]);
  }
  {
    const r = await queryLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'sse'));
    const n = await queryLatency(() => startHttp(NODE_HTTP, stubUrl, port, {}, agentA, 'sse'));
    rows.push(['http SSE SN-Query-Table p50 (ms) [a]', r.p50.toFixed(2), n.p50.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p50, n.p50)]);
    rows.push(['http SSE SN-Query-Table p99 (ms) [a]', r.p99.toFixed(2), n.p99.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p99, n.p99)]);
  }
  {
    const r = await concurrentMeasure(await httpConcurrentSessions(RUST_HTTP, stubUrl, port, agentA, 'sse'));
    const n = await concurrentMeasure(await httpConcurrentSessions(NODE_HTTP, stubUrl, port, agentA, 'sse'));
    rows.push(['http SSE 8x conc throughput (calls/s) [a]', r.throughput.toFixed(1), n.throughput.toFixed(1), errCell(r.errors, n.errors), flagBetterIsMore(r.throughput, n.throughput)]);
    rows.push(['http SSE 8x conc p99 (ms) [a]', r.p99.toFixed(2), n.p99.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p99, n.p99)]);
    infoLines.push('http SSE [a] 8x conc: Rust distinct session ids = ' + r.distinctIds + '/' + CONCURRENT_CLIENTS + '; Node = ' + n.distinctIds + '/' + CONCURRENT_CLIENTS);
  }

  // -------- http variant (b): keepAlive agent, legacy SSE --------
  const agentB = new http.Agent({ keepAlive: true, maxSockets: 8 });
  {
    const r = await coldStartHttp(RUST_HTTP, stubUrl, port, {}, agentB, 'sse');
    const n = await coldStartHttp(NODE_HTTP, stubUrl, port, {}, agentB, 'sse');
    rows.push(['http SSE cold start (ms, median) [b: keepAlive]', r.ms.toFixed(1), n.ms.toFixed(1), errCell(r.errors, n.errors), flagWorse(r.ms, n.ms)]);
  }
  {
    const r = await rssMeasure(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentB, 'sse'), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    const n = await rssMeasure(() => startHttp(NODE_HTTP, stubUrl, port, {}, agentB, 'sse'), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    rows.push(['http SSE idle RSS (KB) [b]', r.idle.toFixed(0), n.idle.toFixed(0), errCell(r.errors, n.errors), flagWorse(r.idle, n.idle)]);
    rows.push(['http SSE RSS after 1000 calls (KB) [b]', r.after.toFixed(0), n.after.toFixed(0), errCell(r.errors, n.errors), flagWorse(r.after, n.after)]);
  }
  {
    const r = await listLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentB, 'sse'));
    const n = await listLatency(() => startHttp(NODE_HTTP, stubUrl, port, {}, agentB, 'sse'));
    rows.push(['http SSE tools/list p50 (ms) [b]', r.p50.toFixed(2), n.p50.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p50, n.p50)]);
    rows.push(['http SSE tools/list p99 (ms) [b]', r.p99.toFixed(2), n.p99.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p99, n.p99)]);
  }
  {
    const r = await queryLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentB, 'sse'));
    const n = await queryLatency(() => startHttp(NODE_HTTP, stubUrl, port, {}, agentB, 'sse'));
    rows.push(['http SSE SN-Query-Table p50 (ms) [b]', r.p50.toFixed(2), n.p50.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p50, n.p50)]);
    rows.push(['http SSE SN-Query-Table p99 (ms) [b]', r.p99.toFixed(2), n.p99.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p99, n.p99)]);
  }
  {
    const r = await concurrentMeasure(await httpConcurrentSessions(RUST_HTTP, stubUrl, port, agentB, 'sse'));
    const n = await concurrentMeasure(await httpConcurrentSessions(NODE_HTTP, stubUrl, port, agentB, 'sse'));
    rows.push(['http SSE 8x conc throughput (calls/s) [b]', r.throughput.toFixed(1), n.throughput.toFixed(1), errCell(r.errors, n.errors), flagBetterIsMore(r.throughput, n.throughput)]);
    rows.push(['http SSE 8x conc p99 (ms) [b]', r.p99.toFixed(2), n.p99.toFixed(2), errCell(r.errors, n.errors), flagWorse(r.p99, n.p99)]);
    infoLines.push('http SSE [b] 8x conc: Rust distinct session ids = ' + r.distinctIds + '/' + CONCURRENT_CLIENTS + '; Node = ' + n.distinctIds + '/' + CONCURRENT_CLIENTS);
  }
  agentB.destroy();

  // -------- http Streamable session mode (2025-11-25), variant (a) --------
  {
    const r = await coldStartHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'session');
    rows.push(['http Streamable-session cold start (ms, median) [a]', r.ms.toFixed(1), na, errCell(r.errors, '-'), flagWorse(r.ms, 0)]);
  }
  {
    const r = await rssMeasure(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'session'), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    rows.push(['http Streamable-session idle RSS (KB) [a]', r.idle.toFixed(0), na, errCell(r.errors, '-'), flagWorse(r.idle, 0)]);
    rows.push(['http Streamable-session RSS after 1000 calls (KB) [a]', r.after.toFixed(0), na, errCell(r.errors, '-'), flagWorse(r.after, 0)]);
  }
  {
    const r = await listLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'session'));
    rows.push(['http Streamable-session tools/list p50 (ms) [a]', r.p50.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p50, 0)]);
    rows.push(['http Streamable-session tools/list p99 (ms) [a]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
  }
  {
    const r = await queryLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'session'));
    rows.push(['http Streamable-session SN-Query-Table p50 (ms) [a]', r.p50.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p50, 0)]);
    rows.push(['http Streamable-session SN-Query-Table p99 (ms) [a]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
  }
  {
    const r = await concurrentMeasure(await httpConcurrentSessions(RUST_HTTP, stubUrl, port, agentA, 'session'));
    rows.push(['http Streamable-session 8x conc throughput (calls/s) [a]', r.throughput.toFixed(1), na, errCell(r.errors, '-'), flagBetterIsMore(r.throughput, 0)]);
    rows.push(['http Streamable-session 8x conc p99 (ms) [a]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
    infoLines.push('http Streamable-session [a] 8x conc: Rust distinct session ids = ' + r.distinctIds + '/' + CONCURRENT_CLIENTS + '; Node = n/a (no Streamable HTTP)');
  }

  // -------- http Streamable session mode (2025-11-25), variant (b) --------
  const agentB2 = new http.Agent({ keepAlive: true, maxSockets: 8 });
  {
    const r = await coldStartHttp(RUST_HTTP, stubUrl, port, {}, agentB2, 'session');
    rows.push(['http Streamable-session cold start (ms, median) [b]', r.ms.toFixed(1), na, errCell(r.errors, '-'), flagWorse(r.ms, 0)]);
  }
  {
    const r = await rssMeasure(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentB2, 'session'), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    rows.push(['http Streamable-session idle RSS (KB) [b]', r.idle.toFixed(0), na, errCell(r.errors, '-'), flagWorse(r.idle, 0)]);
    rows.push(['http Streamable-session RSS after 1000 calls (KB) [b]', r.after.toFixed(0), na, errCell(r.errors, '-'), flagWorse(r.after, 0)]);
  }
  {
    const r = await listLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentB2, 'session'));
    rows.push(['http Streamable-session tools/list p50 (ms) [b]', r.p50.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p50, 0)]);
    rows.push(['http Streamable-session tools/list p99 (ms) [b]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
  }
  {
    const r = await queryLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentB2, 'session'));
    rows.push(['http Streamable-session SN-Query-Table p50 (ms) [b]', r.p50.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p50, 0)]);
    rows.push(['http Streamable-session SN-Query-Table p99 (ms) [b]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
  }
  {
    const r = await concurrentMeasure(await httpConcurrentSessions(RUST_HTTP, stubUrl, port, agentB2, 'session'));
    rows.push(['http Streamable-session 8x conc throughput (calls/s) [b]', r.throughput.toFixed(1), na, errCell(r.errors, '-'), flagBetterIsMore(r.throughput, 0)]);
    rows.push(['http Streamable-session 8x conc p99 (ms) [b]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
    infoLines.push('http Streamable-session [b] 8x conc: Rust distinct session ids = ' + r.distinctIds + '/' + CONCURRENT_CLIENTS + '; Node = n/a (no Streamable HTTP)');
  }
  agentB2.destroy();

  // -------- http Streamable stateless mode (2026-07-28), variant (a) --------
  {
    const r = await coldStartHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'stateless');
    rows.push(['http Streamable-stateless cold start (ms, median) [a]', r.ms.toFixed(1), na, errCell(r.errors, '-'), flagWorse(r.ms, 0)]);
  }
  {
    const r = await rssMeasure(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'stateless'), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    rows.push(['http Streamable-stateless idle RSS (KB) [a]', r.idle.toFixed(0), na, errCell(r.errors, '-'), flagWorse(r.idle, 0)]);
    rows.push(['http Streamable-stateless RSS after 1000 calls (KB) [a]', r.after.toFixed(0), na, errCell(r.errors, '-'), flagWorse(r.after, 0)]);
  }
  {
    const r = await listLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'stateless'));
    rows.push(['http Streamable-stateless tools/list p50 (ms) [a]', r.p50.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p50, 0)]);
    rows.push(['http Streamable-stateless tools/list p99 (ms) [a]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
  }
  {
    const r = await queryLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentA, 'stateless'));
    rows.push(['http Streamable-stateless SN-Query-Table p50 (ms) [a]', r.p50.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p50, 0)]);
    rows.push(['http Streamable-stateless SN-Query-Table p99 (ms) [a]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
  }
  {
    const r = await concurrentMeasure(await httpConcurrentSessions(RUST_HTTP, stubUrl, port, agentA, 'stateless'));
    rows.push(['http Streamable-stateless 8x conc throughput (calls/s) [a]', r.throughput.toFixed(1), na, errCell(r.errors, '-'), flagBetterIsMore(r.throughput, 0)]);
    rows.push(['http Streamable-stateless 8x conc p99 (ms) [a]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
    infoLines.push('http Streamable-stateless [a] 8x conc: Rust distinct session ids = N/A (stateless, no session); Node = n/a (no Streamable HTTP)');
  }

  // -------- http Streamable stateless mode (2026-07-28), variant (b) --------
  const agentB3 = new http.Agent({ keepAlive: true, maxSockets: 8 });
  {
    const r = await coldStartHttp(RUST_HTTP, stubUrl, port, {}, agentB3, 'stateless');
    rows.push(['http Streamable-stateless cold start (ms, median) [b]', r.ms.toFixed(1), na, errCell(r.errors, '-'), flagWorse(r.ms, 0)]);
  }
  {
    const r = await rssMeasure(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentB3, 'stateless'), (s) => s.toolsCall('SN-Query-Table', { table_name: 'incident', limit: 5 }));
    rows.push(['http Streamable-stateless idle RSS (KB) [b]', r.idle.toFixed(0), na, errCell(r.errors, '-'), flagWorse(r.idle, 0)]);
    rows.push(['http Streamable-stateless RSS after 1000 calls (KB) [b]', r.after.toFixed(0), na, errCell(r.errors, '-'), flagWorse(r.after, 0)]);
  }
  {
    const r = await listLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentB3, 'stateless'));
    rows.push(['http Streamable-stateless tools/list p50 (ms) [b]', r.p50.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p50, 0)]);
    rows.push(['http Streamable-stateless tools/list p99 (ms) [b]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
  }
  {
    const r = await queryLatency(() => startHttp(RUST_HTTP, stubUrl, port, {}, agentB3, 'stateless'));
    rows.push(['http Streamable-stateless SN-Query-Table p50 (ms) [b]', r.p50.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p50, 0)]);
    rows.push(['http Streamable-stateless SN-Query-Table p99 (ms) [b]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
  }
  {
    const r = await concurrentMeasure(await httpConcurrentSessions(RUST_HTTP, stubUrl, port, agentB3, 'stateless'));
    rows.push(['http Streamable-stateless 8x conc throughput (calls/s) [b]', r.throughput.toFixed(1), na, errCell(r.errors, '-'), flagBetterIsMore(r.throughput, 0)]);
    rows.push(['http Streamable-stateless 8x conc p99 (ms) [b]', r.p99.toFixed(2), na, errCell(r.errors, '-'), flagWorse(r.p99, 0)]);
    infoLines.push('http Streamable-stateless [b] 8x conc: Rust distinct session ids = N/A (stateless, no session); Node = n/a (no Streamable HTTP)');
  }
  agentB3.destroy();

  // -------- size --------
  const rustStdioSize = fileSize(RUST_STDIO);
  const rustHttpSize = fileSize(RUST_HTTP);
  const nodeStdioSize = fileSize(NODE_STDIO);
  const nodeHttpSize = fileSize(NODE_HTTP);
  rows.push(['stdio binary size (bytes)', rustStdioSize.toLocaleString(), nodeStdioSize.toLocaleString(), '-/-', '']);
  rows.push(['http binary size (bytes)', rustHttpSize.toLocaleString(), nodeHttpSize.toLocaleString(), '-/-', '']);

  await stopStubServer();

  // ---- print ----
  console.log('# Perf sanity v3: Rust 7.0 vs Node 6.2.0 (hermetic, loopback stub)');
  console.log();
  console.log('Errors column = Rust errors / Node errors across the phase. n/a = Node 6.2.0 does not support that mode.');
  console.log();
  console.log('| Measure | Rust 7.0 | Node 6.2.0 | Errors R/N | Flag |');
  console.log('|---------|----------|------------|------------|------|');
  for (const row of rows) {
    console.log('| ' + row.join(' | ') + ' |');
  }
  console.log();
  for (const line of infoLines) console.log(line);
  console.log();
  console.log('Flag: \u26a0\ufe0f >2x means the Rust number is worse than 2x the Node number (throughput inverted).');

  const uname = execSync('uname -a', { encoding: 'utf8' }).trim();
  const cores = execSync('sysctl -n hw.ncpu', { encoding: 'utf8' }).trim();

  const v1Rows = [
    ['stdio cold start (ms, median)', '6.9', '136.5', ''],
    ['stdio idle RSS (KB)', '13808', '97824', ''],
    ['stdio RSS after 1000 calls (KB)', '15872', '128960', ''],
    ['stdio tools/list p50 (ms)', '0.42', '0.40', ''],
    ['stdio tools/list p99 (ms)', '0.54', '1.02', ''],
    ['stdio SN-Query-Table p50 (ms)', '0.19', '0.36', ''],
    ['stdio SN-Query-Table p99 (ms)', '0.39', '1.26', ''],
    ['stdio 8x conc throughput (calls/s)', '11343.2', '3131.7', ''],
    ['stdio 8x conc p99 (ms)', '11.52', '35.82', ''],
    ['http cold start (ms, median)', '105.8', '211.9', ''],
    ['http idle RSS (KB)', '15216', '106464', ''],
    ['http RSS after 1000 calls (KB)', '16720', '123488', ''],
    ['http tools/list p50 (ms)', '0.62', '0.21', '\u26a0\ufe0f >2x'],
    ['http tools/list p99 (ms)', '1.01', '0.64', ''],
    ['http SN-Query-Table p50 (ms)', '0.36', '0.89', ''],
    ['http SN-Query-Table p99 (ms)', '0.85', '4.91', ''],
    ['http 8x conc throughput (calls/s)', '3288.8', '7571.4', '\u26a0\ufe0f >2x'],
    ['http 8x conc p99 (ms)', '18.68', '2.92', '\u26a0\ufe0f >2x'],
    ['stdio binary size (bytes)', '12,973,648', '5,258', ''],
    ['http binary size (bytes)', '13,958,592', '3,104', ''],
  ];

  const mdLines = [];
  mdLines.push('# Perf sanity report (P4, lane E)');
  mdLines.push('');
  mdLines.push('Command: `node scripts/perf/run.mjs`');
  mdLines.push('');
  mdLines.push('Machine: `' + uname + '`');
  mdLines.push('Cores: ' + cores);
  mdLines.push('');
  mdLines.push('Both servers run hermetically against the loopback stub `scripts/golden/lib/stub-sn.mjs`;');
  mdLines.push('temp `HOME` and `HAPPY_CONFIG_PATH`, env credentials, HTTP bearer token in memory only.');
  mdLines.push('');
  mdLines.push('## v1, unchecked (first run; no error counting)');
  mdLines.push('');
  mdLines.push('| Measure | Rust 7.0 | Node 6.2.0 | Flag |');
  mdLines.push('|---------|----------|------------|------|');
  for (const vr of v1Rows) {
    mdLines.push('| ' + vr.join(' | ') + ' |');
  }
  mdLines.push('');
  mdLines.push('Flag: \u26a0\ufe0f >2x means the Rust number is worse than 2x the Node number (throughput inverted).');
  mdLines.push('');
  mdLines.push('## v3, verified (error counts; HTTP variant (a) = new TCP per request, (b) = keepAlive agent)');
  mdLines.push('');
  mdLines.push('Errors column = Rust errors / Node errors across the phase. n/a = Node 6.2.0 does not support that mode.');
  mdLines.push('');
  mdLines.push('| Measure | Rust 7.0 | Node 6.2.0 | Errors R/N | Flag |');
  mdLines.push('|---------|----------|------------|------------|------|');
  for (const row of rows) {
    mdLines.push('| ' + row.join(' | ') + ' |');
  }
  mdLines.push('');
  for (const line of infoLines) mdLines.push('- ' + line);
  mdLines.push('');
  mdLines.push('Flag: \u26a0\ufe0f >2x means the Rust number is worse than 2x the Node number (throughput inverted).');
  mdLines.push('');

  mkdirSync('/tmp/hpm-specs/p4', { recursive: true });
  writeFileSync('/tmp/hpm-specs/p4/perf.md', mdLines.join('\n'));
  console.log();
  console.log('Written /tmp/hpm-specs/p4/perf.md');
}

main().catch((e) => {
  console.error('FATAL:', e);
  process.exit(1);
});
