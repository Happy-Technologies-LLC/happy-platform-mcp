#!/usr/bin/env node
// C7 observation: what the server does when the client disconnects mid tools/call, per era.
// The server runs with HPM_S1_DELAY_MS=3000, so the handler is still sleeping when the client aborts.
// Server stderr (event log + DropProbe) is the evidence; no ServiceNow call is made before the abort.
//   node cancel-probe.mjs --bin target/debug/hpm-s1
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const bin = process.argv[process.argv.indexOf('--bin') + 1];
const outDir = path.join(here, '..', 'out');
mkdirSync(outDir, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const MODERN = '2026-07-28';
const META = {
  'io.modelcontextprotocol/protocolVersion': MODERN,
  'io.modelcontextprotocol/clientInfo': { name: 's1-cancel', version: '0' },
  'io.modelcontextprotocol/clientCapabilities': {}
};
const args = { name: 'SN-Query-Table', arguments: { table_name: 'sys_user', limit: 1, fields: 'sys_id' } };
const env = { ...process.env, HPM_S1_DELAY_MS: '3000', SERVICENOW_INSTANCE: 'dev451274' };
for (const k of ['SERVICENOW_INSTANCE_URL', 'SERVICENOW_USERNAME', 'SERVICENOW_PASSWORD']) delete env[k];

const freePort = () => new Promise((resolve) => {
  const s = createServer();
  s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); });
});

function start(mode, extraEnv = {}) {
  const child = spawn(bin, [mode], { env: { ...env, ...extraEnv }, stdio: ['pipe', 'pipe', 'pipe'] });
  const log = [];
  child.stderr.on('data', (d) => log.push(...String(d).split('\n').filter((l) => l.includes('[hpm-s1'))));
  return { child, log };
}

const results = {};
const callLines = (log) => log.filter((l) => /call |drop /.test(l)).map((l) => l.replace(/^\[hpm-s1 \+\s*\d+ms\] /, ''));

// HTTP eras
{
  const port = await freePort();
  const token = randomBytes(32).toString('hex');
  const { child, log } = start('http', { PORT: String(port), HAPPY_MCP_API_TOKEN: token });
  const base = `http://127.0.0.1:${port}`;
  const auth = { Authorization: `Bearer ${token}` };
  for (let i = 0; i < 100; i += 1) { try { if ((await fetch(`${base}/health`, { headers: auth })).ok) break; } catch { /* */ } await sleep(100); }
  const json = { ...auth, 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream' };

  const abortAfter = async (label, url, headers, body) => {
    const before = log.length;
    const ac = new AbortController();
    const p = fetch(`${base}${url}`, { method: 'POST', headers, body: JSON.stringify(body), signal: ac.signal }).then((r) => r.body?.getReader().read()).catch((e) => e.name);
    await sleep(800);
    ac.abort();
    await p;
    await sleep(3500);
    results[label] = callLines(log.slice(before));
  };

  await abortAfter('modern-stateless POST aborted', '/mcp',
    { ...json, 'MCP-Protocol-Version': MODERN, 'Mcp-Method': 'tools/call', 'Mcp-Name': 'SN-Query-Table' },
    { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { ...args, _meta: META } });

  const init = await fetch(`${base}/mcp`, { method: 'POST', headers: { ...json, 'MCP-Protocol-Version': '2025-11-25' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 0, method: 'initialize', params: { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'c', version: '0' } } }) });
  const sid = init.headers.get('mcp-session-id');
  await init.text();
  const sess = { ...json, 'MCP-Protocol-Version': '2025-11-25', 'Mcp-Session-Id': sid };
  await fetch(`${base}/mcp`, { method: 'POST', headers: sess, body: JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) });
  await abortAfter('legacy-session POST aborted (session kept)', '/mcp', sess, { jsonrpc: '2.0', id: 2, method: 'tools/call', params: args });

  // Legacy HTTP+SSE: POST returns 202 at once; disconnect = closing the GET stream.
  const before = log.length;
  const ac = new AbortController();
  const sse = await fetch(`${base}/mcp`, { headers: { ...auth, Accept: 'text/event-stream' }, signal: ac.signal });
  const reader = sse.body.getReader();
  let buf = '';
  while (!/data: ?(\S+)/.test(buf)) buf += new TextDecoder().decode((await reader.read()).value);
  const endpoint = /data: ?(\S+)/.exec(buf)[1];
  const post = (body) => fetch(`${base}${endpoint}`, { method: 'POST', headers: { ...auth, 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  await post({ jsonrpc: '2.0', id: 0, method: 'initialize', params: { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'c', version: '0' } } });
  await post({ jsonrpc: '2.0', method: 'notifications/initialized' });
  await post({ jsonrpc: '2.0', id: 3, method: 'tools/call', params: args });
  await sleep(800);
  ac.abort();
  await sleep(3500);
  results['legacy HTTP+SSE stream closed'] = callLines(log.slice(before));
  child.kill('SIGTERM');
}

// stdio: close stdin mid-call.
{
  const { child, log } = start('stdio');
  const send = (m) => child.stdin.write(`${JSON.stringify(m)}\n`);
  send({ jsonrpc: '2.0', id: 0, method: 'initialize', params: { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'c', version: '0' } } });
  await sleep(300);
  send({ jsonrpc: '2.0', method: 'notifications/initialized' });
  send({ jsonrpc: '2.0', id: 4, method: 'tools/call', params: args });
  await sleep(800);
  child.stdin.end();
  await sleep(3500);
  results['stdio stdin EOF'] = callLines(log);
  child.kill('SIGTERM');
}

writeFileSync(path.join(outDir, 'cancel-probe.json'), `${JSON.stringify(results, null, 2)}\n`);
console.log(JSON.stringify(results, null, 2));
