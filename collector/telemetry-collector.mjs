#!/usr/bin/env node
/**
 * Collector for happy-platform-mcp opt-in usage pings (src/usage-telemetry.js).
 *
 * Dependency-free. Listens on 127.0.0.1 only; a reverse proxy (Tailscale
 * Funnel on Build01) exposes just POST /api/mcp/telemetry.
 *
 * Each accepted ping is appended to DATA_DIR/YYYY-MM-DD.jsonl (UTC) as the
 * validated payload plus `received_at`. The client IP is used only for an
 * in-memory rate limit and is never written anywhere; no headers or user
 * agent are stored.
 *
 *   node telemetry-collector.mjs              # serve
 *   node telemetry-collector.mjs --summarize  # print totals across all files
 *
 * Env: HAPPY_MCP_COLLECTOR_PORT (default 18610),
 *      HAPPY_MCP_COLLECTOR_DATA_DIR (default ./data next to this file).
 */

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HOST = '127.0.0.1';
const PORT = Number(process.env.HAPPY_MCP_COLLECTOR_PORT || 18610);
const DATA_DIR = process.env.HAPPY_MCP_COLLECTOR_DATA_DIR
  || path.join(path.dirname(fileURLToPath(import.meta.url)), 'data');
const ENDPOINT = '/api/mcp/telemetry';
const MAX_BODY_BYTES = 16 * 1024;
const MAX_TOOL_KEYS = 600;
const MAX_TOOL_COUNT = 1_000_000;
const RATE_WINDOW_MS = 60_000;
const RATE_PER_IP = 60;
const RATE_GLOBAL = 1200;

const PAYLOAD_KEYS = ['v', 'package', 'version', 'session', 'transport', 'node', 'platform', 'tools', 'sent_at'];
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const VERSION_RE = /^\d{1,4}\.\d{1,4}\.\d{1,4}(?:[-+][0-9A-Za-z.-]{1,32})?$/;
const TOKEN_RE = /^[a-z0-9_-]{1,16}$/;
const NODE_RE = /^(?:v\d{1,3}|unknown)$/;
const TOOL_RE = /^[A-Za-z0-9_.:-]{1,128}$/;
const ISO_RE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z$/;

/** Returns the clean payload, or a string describing why it was rejected. */
export function validatePing(body) {
  if (!body || typeof body !== 'object' || Array.isArray(body)) return 'payload must be a JSON object';
  const keys = Object.keys(body);
  const unknown = keys.filter((k) => !PAYLOAD_KEYS.includes(k));
  if (unknown.length) return `unknown fields: ${unknown.slice(0, 5).join(', ')}`;
  const missing = PAYLOAD_KEYS.filter((k) => !(k in body));
  if (missing.length) return `missing fields: ${missing.join(', ')}`;

  const { v, version, session, transport, node, platform, tools, sent_at: sentAt } = body;
  if (v !== 1) return 'v must be 1';
  if (body.package !== 'happy-platform-mcp') return 'package must be happy-platform-mcp';
  if (typeof version !== 'string' || !VERSION_RE.test(version)) return 'version must be a semver string';
  if (typeof session !== 'string' || !UUID_RE.test(session)) return 'session must be a UUID';
  if (typeof transport !== 'string' || !TOKEN_RE.test(transport)) return 'transport is invalid';
  if (typeof node !== 'string' || !NODE_RE.test(node)) return 'node is invalid';
  if (typeof platform !== 'string' || !TOKEN_RE.test(platform)) return 'platform is invalid';
  if (typeof sentAt !== 'string' || !ISO_RE.test(sentAt) || Number.isNaN(Date.parse(sentAt))) {
    return 'sent_at must be an ISO-8601 UTC timestamp';
  }
  if (!tools || typeof tools !== 'object' || Array.isArray(tools)) return 'tools must be an object';
  const toolNames = Object.keys(tools);
  if (toolNames.length === 0) return 'tools must not be empty';
  if (toolNames.length > MAX_TOOL_KEYS) return `tools has more than ${MAX_TOOL_KEYS} keys`;
  const cleanTools = {};
  for (const name of toolNames) {
    const count = tools[name];
    if (!TOOL_RE.test(name)) return 'tool names must match [A-Za-z0-9_.:-]{1,128}';
    if (!Number.isSafeInteger(count) || count < 0 || count > MAX_TOOL_COUNT) {
      return `tool counts must be integers from 0 to ${MAX_TOOL_COUNT}`;
    }
    cleanTools[name] = count;
  }
  return { v, package: body.package, version, session, transport, node, platform, tools: cleanTools, sent_at: sentAt };
}

// Fixed-window rate limit. Keys live only in memory and the map is dropped
// every window, so no client address outlives a minute.
let windowHits = new Map();
let windowTotal = 0;
const sweep = setInterval(() => { windowHits = new Map(); windowTotal = 0; }, RATE_WINDOW_MS);
sweep.unref();

function rateLimited(ip) {
  windowTotal += 1;
  const hits = (windowHits.get(ip) || 0) + 1;
  windowHits.set(ip, hits);
  return hits > RATE_PER_IP || windowTotal > RATE_GLOBAL;
}

function clientAddress(req) {
  const peer = req.socket.remoteAddress || '';
  // Behind the local proxy the peer is loopback; the proxy appends the real
  // client as the last X-Forwarded-For entry.
  if (peer === '127.0.0.1' || peer === '::1' || peer === '::ffff:127.0.0.1') {
    const forwarded = String(req.headers['x-forwarded-for'] || '').split(',').map((s) => s.trim()).filter(Boolean);
    if (forwarded.length) return forwarded[forwarded.length - 1];
  }
  return peer;
}

let writeChain = Promise.resolve();
function store(record) {
  const file = path.join(DATA_DIR, `${record.received_at.slice(0, 10)}.jsonl`);
  const line = `${JSON.stringify(record)}\n`;
  writeChain = writeChain.then(() => fs.promises.appendFile(file, line, { mode: 0o600 }));
  return writeChain;
}

function reply(res, status, message) {
  if (status === 204) {
    res.writeHead(204);
    res.end();
    return;
  }
  const headers = { 'content-type': 'application/json' };
  if (status === 405) headers.allow = 'POST';
  res.writeHead(status, headers);
  res.end(`${JSON.stringify({ error: message })}\n`);
}

function handle(req, res) {
  const pathname = (req.url || '').split('?')[0];
  if (pathname !== ENDPOINT) return reply(res, 404, 'not found');
  if (req.method !== 'POST') return reply(res, 405, 'method not allowed');
  if (rateLimited(clientAddress(req))) return reply(res, 429, 'rate limited');
  const type = String(req.headers['content-type'] || '').split(';')[0].trim().toLowerCase();
  if (type !== 'application/json') return reply(res, 415, 'content-type must be application/json');
  const declared = Number(req.headers['content-length']);
  if (Number.isFinite(declared) && declared > MAX_BODY_BYTES) return reply(res, 413, 'payload too large');

  const chunks = [];
  let size = 0;
  let aborted = false;
  req.on('data', (chunk) => {
    if (aborted) return;
    size += chunk.length;
    if (size > MAX_BODY_BYTES) {
      aborted = true;
      reply(res, 413, 'payload too large');
      req.resume();
      return;
    }
    chunks.push(chunk);
  });
  req.on('end', () => {
    if (aborted) return;
    let parsed;
    try {
      parsed = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    } catch {
      return reply(res, 400, 'invalid JSON');
    }
    const result = validatePing(parsed);
    if (typeof result === 'string') return reply(res, 400, result);
    store({ received_at: new Date().toISOString(), ...result }).then(
      () => reply(res, 204),
      (err) => {
        console.error(`write failed: ${err.message}`);
        reply(res, 500, 'storage error');
      }
    );
  });
  req.on('error', () => { aborted = true; });
}

function serve() {
  fs.mkdirSync(DATA_DIR, { recursive: true, mode: 0o700 });
  const server = http.createServer(handle);
  server.requestTimeout = 10_000;
  server.headersTimeout = 5_000;
  server.keepAliveTimeout = 5_000;
  server.listen(PORT, HOST, () => {
    console.log(`happy-mcp telemetry collector on http://${HOST}:${PORT}${ENDPOINT}, data in ${DATA_DIR}`);
  });
  const shutdown = () => server.close(() => process.exit(0));
  process.on('SIGTERM', shutdown);
  process.on('SIGINT', shutdown);
}

function summarize() {
  const files = fs.existsSync(DATA_DIR)
    ? fs.readdirSync(DATA_DIR).filter((f) => /^\d{4}-\d{2}-\d{2}\.jsonl$/.test(f)).sort()
    : [];
  const tools = new Map();
  const dims = { version: new Map(), platform: new Map(), transport: new Map(), node: new Map() };
  const sessions = new Set();
  let pings = 0;
  for (const file of files) {
    for (const line of fs.readFileSync(path.join(DATA_DIR, file), 'utf8').split('\n')) {
      if (!line.trim()) continue;
      let ping;
      try { ping = JSON.parse(line); } catch { continue; }
      pings += 1;
      sessions.add(ping.session);
      for (const [name, count] of Object.entries(ping.tools || {})) tools.set(name, (tools.get(name) || 0) + count);
      for (const [dim, counts] of Object.entries(dims)) {
        // Count distinct sessions per value, so long-running processes don't dominate.
        const seen = counts.get(ping[dim]) || new Set();
        seen.add(ping.session);
        counts.set(ping[dim], seen);
      }
    }
  }
  const range = files.length ? `${files[0].slice(0, 10)} .. ${files[files.length - 1].slice(0, 10)}` : 'no data';
  console.log(`Files: ${files.length} (${range})  Pings: ${pings}  Sessions: ${sessions.size}`);
  const total = [...tools.values()].reduce((a, b) => a + b, 0);
  console.log(`\nTool calls: ${total} across ${tools.size} tools`);
  for (const [name, count] of [...tools.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))) {
    console.log(`  ${String(count).padStart(8)}  ${name}`);
  }
  for (const [dim, counts] of Object.entries(dims)) {
    console.log(`\nSessions by ${dim}:`);
    for (const [value, seen] of [...counts.entries()].sort((a, b) => b[1].size - a[1].size)) {
      console.log(`  ${String(seen.size).padStart(8)}  ${value}`);
    }
  }
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.argv.includes('--summarize')) summarize();
  else serve();
}
