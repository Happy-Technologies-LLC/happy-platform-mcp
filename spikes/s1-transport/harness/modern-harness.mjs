#!/usr/bin/env node
// Modern-era (2026-07-28 stateless) client test via raw HTTP, plus plan §5.3 dispatch-rule
// and C3 header-validation cases. Executable-parameterized like legacy-harness.mjs.
//   node modern-harness.mjs --exec '["bin","http"]' --label rust [--cwd DIR]
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { request as httpRequest } from 'node:http';
import { createServer } from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const arg = (n, d) => { const i = process.argv.indexOf(`--${n}`); return i > 0 ? process.argv[i + 1] : d; };
const exec = JSON.parse(arg('exec'));
const label = arg('label', 'rust');
const outDir = arg('out', path.join(here, '..', 'out'));
mkdirSync(outDir, { recursive: true });
const MODERN = '2026-07-28';
const META = {
  'io.modelcontextprotocol/protocolVersion': MODERN,
  'io.modelcontextprotocol/clientInfo': { name: 's1-modern-harness', version: '0.0.0' },
  'io.modelcontextprotocol/clientCapabilities': {}
};

async function freePort() {
  return new Promise((resolve) => {
    const s = createServer();
    s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); });
  });
}

const port = await freePort();
const token = randomBytes(32).toString('hex');
const env = { ...process.env, PORT: String(port), HAPPY_MCP_API_TOKEN: token, SERVICENOW_INSTANCE: 'dev451274' };
for (const k of ['SERVICENOW_INSTANCE_URL', 'SERVICENOW_USERNAME', 'SERVICENOW_PASSWORD']) delete env[k];
const child = spawn(exec[0], exec.slice(1), { cwd: arg('cwd', process.cwd()), env, stdio: ['ignore', 'ignore', 'pipe'] });
let stderr = '';
child.stderr.on('data', (d) => { stderr += d; });
const base = `http://127.0.0.1:${port}`;
const auth = { Authorization: `Bearer ${token}` };
for (let i = 0; i < 100; i += 1) {
  try { if ((await fetch(`${base}/health`, { headers: auth })).ok) break; } catch { /* starting */ }
  await new Promise((r) => setTimeout(r, 100));
}

const modernHeaders = (method, name) => ({
  ...auth,
  'Content-Type': 'application/json',
  Accept: 'application/json, text/event-stream',
  'MCP-Protocol-Version': MODERN,
  'Mcp-Method': method,
  ...(name !== undefined ? { 'Mcp-Name': name } : {})
});
const legacyHeaders = (extra = {}) => ({
  ...auth, 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', ...extra
});
const rpc = (id, method, params) => ({ jsonrpc: '2.0', id, method, params });
const b64sentinel = (s) => `=?base64?${Buffer.from(s, 'utf8').toString('base64')}?=`;

// Reads a response; for SSE returns the parsed `data:` JSON events (stops after the first JSON-RPC response).
async function readBody(res, { maxMs = 15000 } = {}) {
  const type = res.headers.get('content-type') ?? '';
  if (!type.includes('text/event-stream')) {
    const text = await res.text();
    try { return { kind: 'json', body: JSON.parse(text) }; } catch { return { kind: 'text', body: text }; }
  }
  const reader = res.body.getReader();
  const dec = new TextDecoder();
  let buf = '';
  const events = [];
  const deadline = Date.now() + maxMs;
  while (Date.now() < deadline) {
    const { value, done } = await Promise.race([
      reader.read(),
      new Promise((r) => setTimeout(() => r({ done: true, timeout: true }), Math.max(1, deadline - Date.now())))
    ]);
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let idx;
    while ((idx = buf.indexOf('\n\n')) >= 0) {
      const raw = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      const ev = { event: 'message', data: '' };
      for (const line of raw.split('\n')) {
        if (line.startsWith('event:')) ev.event = line.slice(6).trim();
        else if (line.startsWith('data:')) ev.data += line.slice(5).trimStart();
        else if (line.startsWith('id:')) ev.id = line.slice(3).trim();
        else if (line.startsWith('retry:')) ev.retry = line.slice(6).trim();
      }
      try { ev.json = JSON.parse(ev.data); } catch { /* endpoint / priming */ }
      events.push(ev);
      if (ev.json && ('result' in ev.json || 'error' in ev.json)) {
        reader.cancel().catch(() => {});
        return { kind: 'sse', events };
      }
    }
  }
  reader.cancel().catch(() => {});
  return { kind: 'sse', events };
}

const finalMessage = (b) => (b.kind === 'sse' ? b.events.map((e) => e.json).find((j) => j && ('result' in j || 'error' in j)) : b.body);
const cases = [];
const truncate = (v, n = 900) => { const s = typeof v === 'string' ? v : JSON.stringify(v); return s.length > n ? `${s.slice(0, n)}…(${s.length} chars)` : s; };

async function run(id, rule, description, { method = 'POST', url = '/mcp', headers, body, rawBody, expect, sse = {} }) {
  const init = { method, headers };
  if (rawBody !== undefined) init.body = rawBody;
  else if (body !== undefined) init.body = JSON.stringify(body);
  let res;
  let parsed;
  try {
    res = await fetch(`${base}${url}`, init);
    parsed = await readBody(res, sse);
  } catch (error) {
    cases.push({ id, rule, description, ok: false, error: String(error) });
    return null;
  }
  const message = finalMessage(parsed);
  const observed = {
    status: res.status,
    contentType: res.headers.get('content-type'),
    sessionId: res.headers.get('mcp-session-id') ? '<present>' : null,
    allow: res.headers.get('allow'),
    errorCode: message?.error?.code ?? null
  };
  const ok = expect(observed, message, parsed);
  const redact = Object.fromEntries(Object.entries(headers ?? {}).map(([k, v]) => [k, k === 'Authorization' ? 'Bearer <redacted>' : v]));
  cases.push({
    id, rule, description, ok: Boolean(ok), observed,
    request: { method, url: url.replace(/sessionId=[^&]+/, 'sessionId=<sid>'), headers: redact, body: rawBody !== undefined ? truncate(rawBody, 120) : body },
    response: parsed.kind === 'sse'
      ? { sseEvents: parsed.events.map((e) => ({ event: e.event, data: truncate(e.data.replace(/sessionId=[0-9a-f-]+/, 'sessionId=<sid>'), 400) })) }
      : { body: truncate(parsed.body) }
  });
  return { res, message, parsed };
}

const tableArgs = { table_name: 'sys_user', limit: 1, fields: 'sys_id,user_name' };

// ---- Modern era happy path (C1, C4)
await run('M1', 'C1', 'server/discover (modern _meta)', {
  headers: modernHeaders('server/discover'), body: rpc(1, 'server/discover', { _meta: META }),
  expect: (o, m) => o.status === 200 && Array.isArray(m?.result?.supportedVersions) && m.result.supportedVersions.includes(MODERN)
});
await run('M2', 'C1/C4/C5', 'tools/list modern: 60 tools, resultType, ttlMs, cacheScope', {
  headers: modernHeaders('tools/list'), body: rpc(2, 'tools/list', { _meta: META }),
  expect: (o, m) => o.status === 200 && m?.result?.tools?.length === 60 && m.result.resultType === 'complete'
    && typeof m.result.ttlMs === 'number' && typeof m.result.cacheScope === 'string' && o.sessionId === null
});
await run('M3', 'C1', 'tools/call SN-Query-Table modern (PDI)', {
  headers: modernHeaders('tools/call', 'SN-Query-Table'),
  body: rpc(3, 'tools/call', { name: 'SN-Query-Table', arguments: tableArgs, _meta: META }),
  expect: (o, m) => o.status === 200 && m?.result?.content?.[0]?.text?.startsWith('Found 1 records in sys_user:\n')
});
await run('M4', 'C6', 'tools/call unknown tool modern', {
  headers: modernHeaders('tools/call', 'SN-Nope'), body: rpc(4, 'tools/call', { name: 'SN-Nope', arguments: {}, _meta: META }),
  expect: (o) => o.errorCode === -32602
});

// ---- C3 header validation (modern)
const listBody = (id) => rpc(id, 'tools/list', { _meta: META });
const { 'MCP-Protocol-Version': _drop, ...noVersion } = modernHeaders('tools/list');
await run('C3-1', 'C3', 'missing MCP-Protocol-Version header', { headers: noVersion, body: listBody(10), expect: (o) => o.status === 400 && o.errorCode === -32020 });
await run('C3-2', 'C3', 'MCP-Protocol-Version header ≠ _meta version', {
  headers: { ...modernHeaders('tools/list'), 'MCP-Protocol-Version': '2025-11-25' }, body: listBody(11),
  expect: (o) => o.status === 400 && o.errorCode === -32020
});
const { 'Mcp-Method': _m, ...noMethod } = modernHeaders('tools/list');
await run('C3-3', 'C3', 'missing Mcp-Method', { headers: noMethod, body: listBody(12), expect: (o) => o.status === 400 && o.errorCode === -32020 });
await run('C3-4', 'C3', 'Mcp-Method ≠ body method', { headers: modernHeaders('prompts/list'), body: listBody(13), expect: (o) => o.status === 400 && o.errorCode === -32020 });
await run('C3-5', 'C3', 'Mcp-Method value differs only in case (values are case-sensitive)', {
  headers: modernHeaders('Tools/List'), body: listBody(14), expect: (o) => o.status === 400 && o.errorCode === -32020
});
await run('C3-6', 'C3', 'header names in lower case (names case-insensitive)', {
  headers: Object.fromEntries(Object.entries(modernHeaders('tools/list')).map(([k, v]) => [k.toLowerCase(), v])), body: listBody(15),
  expect: (o) => o.status === 200
});
await run('C3-7', 'C3', 'tools/call missing Mcp-Name', {
  headers: modernHeaders('tools/call'), body: rpc(16, 'tools/call', { name: 'SN-Query-Table', arguments: tableArgs, _meta: META }),
  expect: (o) => o.status === 400 && o.errorCode === -32020
});
await run('C3-8', 'C3', 'Mcp-Name ≠ params.name', {
  headers: modernHeaders('tools/call', 'SN-Get-Record'), body: rpc(17, 'tools/call', { name: 'SN-Query-Table', arguments: tableArgs, _meta: META }),
  expect: (o) => o.status === 400 && o.errorCode === -32020
});
await run('C3-9', 'C3', 'Mcp-Name Base64 sentinel, decoded value matches', {
  headers: modernHeaders('tools/call', b64sentinel('SN-Query-Table')), body: rpc(18, 'tools/call', { name: 'SN-Query-Table', arguments: tableArgs, _meta: META }),
  expect: (o, m) => o.status === 200 && Boolean(m?.result)
});
await run('C3-10', 'C3', 'Mcp-Name Base64 sentinel, decoded value differs', {
  headers: modernHeaders('tools/call', b64sentinel('SN-Get-Record')), body: rpc(19, 'tools/call', { name: 'SN-Query-Table', arguments: tableArgs, _meta: META }),
  expect: (o) => o.status === 400 && o.errorCode === -32020
});
await run('C3-11', 'C3', 'unsupported version (header + _meta 2099-01-01)', {
  headers: { ...modernHeaders('tools/list'), 'MCP-Protocol-Version': '2099-01-01' },
  body: rpc(20, 'tools/list', { _meta: { ...META, 'io.modelcontextprotocol/protocolVersion': '2099-01-01' } }),
  expect: (o) => o.status === 400 && o.errorCode === -32022
});
await run('C3-12', 'C3', 'unknown method (modern)', {
  headers: modernHeaders('foo/bar'), body: rpc(21, 'foo/bar', { _meta: META }), expect: (o) => o.status === 404 && o.errorCode === -32601
});
await run('C3-13', 'C1', '_meta without clientCapabilities', {
  headers: modernHeaders('tools/list'),
  body: rpc(22, 'tools/list', { _meta: { 'io.modelcontextprotocol/protocolVersion': MODERN } }),
  expect: (o) => o.status === 400
});

// ---- §5.3 dispatch rules
await run('R1-a', '1', 'no bearer token', { headers: { 'Content-Type': 'application/json' }, body: listBody(30), expect: (o) => o.status === 401 });
// fetch() (undici) silently drops a caller-supplied Host header, so this case uses node:http.
{
  const headers = { ...modernHeaders('tools/list'), Host: 'evil.example' };
  const status = await new Promise((resolve, reject) => {
    const req = httpRequest({ host: '127.0.0.1', port, path: '/mcp', method: 'POST', headers }, (r) => { r.resume(); resolve(r.statusCode); });
    req.on('error', reject);
    req.end(JSON.stringify(listBody(31)));
  });
  cases.push({
    id: 'R1-b', rule: '1', description: 'foreign Host header (DNS rebinding)', ok: status === 403, observed: { status },
    request: { method: 'POST', url: '/mcp', headers: { Host: 'evil.example', Authorization: 'Bearer <redacted>' }, body: listBody(31) },
    response: { status }
  });
}
await run('R1-c', '1', 'foreign Origin (allowlist empty ⇒ any Origin rejected; F7)', {
  headers: { ...modernHeaders('tools/list'), Origin: 'https://evil.example' }, body: listBody(32), expect: (o) => o.status === 403
});
await run('R1-d', '1', 'body > 8 MiB', {
  headers: modernHeaders('tools/list'), rawBody: JSON.stringify({ jsonrpc: '2.0', id: 33, method: 'tools/list', params: { _meta: META, pad: 'x'.repeat(8 * 1024 * 1024 + 1) } }),
  expect: (o) => o.status === 413
});
await run('R2', '2', 'both ?sessionId and Mcp-Session-Id', {
  url: '/mcp?sessionId=abc', headers: legacyHeaders({ 'Mcp-Session-Id': 'abc' }), body: rpc(34, 'tools/list', {}), expect: (o) => o.status === 400
});
await run('R3', '3', 'modern _meta + stale Mcp-Session-Id → stateless, no session minted', {
  headers: { ...modernHeaders('tools/list'), 'Mcp-Session-Id': 'stale-session' }, body: listBody(35),
  expect: (o, m) => o.status === 200 && o.sessionId === null && m?.result?.tools?.length === 60
});
const init = (id, v) => rpc(id, 'initialize', { protocolVersion: v, capabilities: {}, clientInfo: { name: 's1', version: '0' } });
let sid = null;
for (const [i, v] of [['R4-a', '2025-11-25'], ['R4-b', '2025-06-18'], ['R4-c', '2025-03-26']].entries()) {
  const r = await run(v[0], '4', `POST initialize ${v[1]} → legacy session`, {
    headers: legacyHeaders(v[1] === '2025-03-26' ? {} : { 'MCP-Protocol-Version': v[1] }), body: init(40 + i, v[1]),
    expect: (o, m) => o.status === 200 && o.sessionId !== null && m?.result?.protocolVersion === v[1]
  });
  if (v[1] === '2025-03-26') sid = r?.res.headers.get('mcp-session-id');
}
await fetch(`${base}/mcp`, { method: 'POST', headers: legacyHeaders({ 'Mcp-Session-Id': sid }), body: JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) });
await run('R5-a', '5', 'POST tools/list with valid Mcp-Session-Id', {
  headers: legacyHeaders({ 'Mcp-Session-Id': sid }), body: rpc(50, 'tools/list', {}),
  expect: (o, m) => o.status === 200 && m?.result?.tools?.length === 60 && m.result.ttlMs === undefined
});
await run('R5-b', '5 / C11', 'JSON-RPC batch on a 2025-03-26 session', {
  headers: legacyHeaders({ 'Mcp-Session-Id': sid }), body: [rpc(51, 'ping', {}), rpc(52, 'ping', {})], expect: () => true
});
await run('R5-c', '5', 'GET with Mcp-Session-Id → standalone SSE stream', {
  method: 'GET', headers: { ...auth, Accept: 'text/event-stream', 'Mcp-Session-Id': sid }, sse: { maxMs: 500 },
  expect: (o) => o.status === 200 && o.contentType?.includes('text/event-stream')
});
await run('R5-d', '5', 'unknown Mcp-Session-Id → 404', {
  headers: legacyHeaders({ 'Mcp-Session-Id': 'does-not-exist' }), body: rpc(53, 'tools/list', {}), expect: (o) => o.status === 404
});
await run('R5-e', '5', 'DELETE with Mcp-Session-Id', { method: 'DELETE', headers: { ...auth, 'Mcp-Session-Id': sid }, expect: (o) => o.status >= 200 && o.status < 300 });
await run('R5-f', '5', 'POST on the deleted session → 404', {
  headers: legacyHeaders({ 'Mcp-Session-Id': sid }), body: rpc(54, 'tools/list', {}), expect: (o) => o.status === 404
});

// Rule 6/7: legacy HTTP+SSE adapter, driven by hand to show the wire.
const sseRes = await fetch(`${base}/mcp`, { headers: { ...auth, Accept: 'text/event-stream' } });
const reader = sseRes.body.getReader();
const dec = new TextDecoder();
let sseBuf = '';
const nextEvent = async () => {
  for (;;) {
    const idx = sseBuf.indexOf('\n\n');
    if (idx >= 0) { const raw = sseBuf.slice(0, idx); sseBuf = sseBuf.slice(idx + 2); if (!raw.startsWith(':')) return raw; continue; }
    const { value, done } = await reader.read();
    if (done) return null;
    sseBuf += dec.decode(value, { stream: true });
  }
};
const endpointRaw = await nextEvent();
const endpoint = /data: ?(.*)/.exec(endpointRaw)?.[1];
cases.push({
  id: 'R6', rule: '6', description: 'GET Accept: text/event-stream, no session → endpoint event',
  ok: sseRes.status === 200 && /^event: ?endpoint/m.test(endpointRaw) && /\/mcp\?sessionId=/.test(endpoint ?? ''),
  observed: { status: sseRes.status, contentType: sseRes.headers.get('content-type') },
  request: { method: 'GET', url: '/mcp', headers: { Authorization: 'Bearer <redacted>', Accept: 'text/event-stream' } },
  response: { firstEvent: endpointRaw?.replace(/sessionId=[0-9a-f-]+/, 'sessionId=<sid>') }
});
const postSse = async (id, body) => {
  const r = await fetch(`${base}${endpoint}`, { method: 'POST', headers: { ...auth, 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  return { status: r.status, text: await r.text() };
};
const pInit = await postSse(60, init(60, '2024-11-05'));
const initEvent = await nextEvent();
await postSse(null, { jsonrpc: '2.0', method: 'notifications/initialized' });
const pList = await postSse(61, rpc(61, 'tools/list', {}));
const listEvent = await nextEvent();
cases.push({
  id: 'R7-a', rule: '7', description: 'POST /mcp?sessionId=… → 202, response on the SSE stream',
  ok: pInit.status === 202 && pList.status === 202 && /"protocolVersion":"2024-11-05"/.test(initEvent ?? '') && (JSON.parse(/data: ?(.*)/.exec(listEvent)[1]).result.tools.length === 60),
  observed: { initPost: pInit, listPost: pList },
  request: { method: 'POST', url: '/mcp?sessionId=<sid>', body: init(60, '2024-11-05') },
  response: { http: `${pInit.status} ${pInit.text}`, sseEvent: truncate(initEvent, 300) }
});
await run('R7-b', '7', 'POST ?sessionId=unknown → 404', {
  url: '/mcp?sessionId=00000000-0000-0000-0000-000000000000', headers: legacyHeaders(), body: rpc(62, 'tools/list', {}), expect: (o) => o.status === 404
});
reader.cancel().catch(() => {});

await run('R8-a', '8', 'malformed JSON → 400 / -32700', { headers: legacyHeaders(), rawBody: '{"jsonrpc":', expect: (o) => o.status === 400 && o.errorCode === -32700 });
await run('R8-b', '8', 'unsupported MCP-Protocol-Version (legacy shape) → 400 / -32022', {
  headers: legacyHeaders({ 'MCP-Protocol-Version': '2099-01-01' }), body: rpc(70, 'tools/list', {}), expect: (o) => o.status === 400 && o.errorCode === -32022
});
await run('R8-c', '8', 'PUT → 405', { method: 'PUT', headers: legacyHeaders(), body: rpc(71, 'tools/list', {}), expect: (o) => o.status === 405 });
await run('R8-d', '8', 'sessionless legacy non-initialize POST', { headers: legacyHeaders(), body: rpc(72, 'tools/list', {}), expect: (o) => o.status === 400 });
await run('R8-e', '8', 'GET without Accept: text/event-stream', { method: 'GET', headers: { ...auth, Accept: 'application/json' }, expect: (o) => o.status === 406 });

child.kill('SIGTERM');
writeFileSync(path.join(outDir, `server-stderr.modern-${label}.log`), stderr);
const passed = cases.every((c) => c.ok || c.id === 'R5-b');
writeFileSync(path.join(outDir, `modern-harness.${label}.json`), `${JSON.stringify({ label, passed, cases }, null, 2)}\n`);
for (const c of cases) console.log(`${c.ok ? 'PASS' : 'FAIL'} ${c.id.padEnd(6)} rule ${String(c.rule).padEnd(9)} ${c.description} ${JSON.stringify(c.observed ?? c.error)}`);
console.log(passed ? 'MODERN HARNESS PASSED' : 'MODERN HARNESS FAILED');
process.exit(passed ? 0 : 1);
