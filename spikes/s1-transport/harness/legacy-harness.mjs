#!/usr/bin/env node
// Executable-parameterized transport harness (plan §7.2 rule 7), legacy era.
// Uses the repo's installed @modelcontextprotocol/sdk client; never imports server internals.
//
//   node legacy-harness.mjs --transport stdio|sse|streamable --exec '["bin","arg"]' [--cwd DIR] [--out DIR]
//
// stdio: spawns --exec as the MCP server. sse/streamable: spawns --exec as an HTTP server with
// PORT and HAPPY_MCP_API_TOKEN set, waits for GET /health, then connects to /mcp.
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { SSEClientTransport } from '@modelcontextprotocol/sdk/client/sse.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import { normalizeToolsList } from './normalize.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : fallback;
};
const transportKind = arg('transport');
const exec = JSON.parse(arg('exec'));
const cwd = arg('cwd', process.cwd());
const label = arg('label', `${path.basename(exec[exec.length - 1])}-${transportKind}`);
// Live-instance safety: pin the PDI, drop legacy .env-style credential vars, refuse customer names.
const instance = arg('instance', 'dev451274');
if (/^ccf/i.test(instance)) throw new Error('refusing customer instance');
const childEnv = { ...process.env, SERVICENOW_INSTANCE: instance };
for (const k of ['SERVICENOW_INSTANCE_URL', 'SERVICENOW_USERNAME', 'SERVICENOW_PASSWORD']) delete childEnv[k];
const outDir = arg('out', path.join(here, '..', 'out'));
mkdirSync(outDir, { recursive: true });
const fixture = readFileSync(path.join(here, '..', 'fixtures', 'tools-list-6.2.0.json'), 'utf8');

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok: Boolean(ok), detail });
};

async function freePort() {
  return new Promise((resolve) => {
    const s = createServer();
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

async function startHttpServer() {
  const port = await freePort();
  const token = randomBytes(32).toString('hex');
  const child = spawn(exec[0], exec.slice(1), {
    cwd,
    env: { ...childEnv, PORT: String(port), HAPPY_MCP_API_TOKEN: token },
    stdio: ['ignore', 'ignore', 'pipe']
  });
  let stderr = '';
  child.stderr.on('data', (d) => { stderr += d; });
  const base = `http://127.0.0.1:${port}`;
  for (let i = 0; i < 100; i += 1) {
    try {
      const r = await fetch(`${base}/health`, { headers: { Authorization: `Bearer ${token}` } });
      if (r.ok) return { child, base, token, stderr: () => stderr };
    } catch { /* not up yet */ }
    await new Promise((r) => setTimeout(r, 100));
  }
  child.kill();
  throw new Error(`server did not become healthy: ${stderr.slice(-500)}`);
}

async function main() {
  let server = null;
  let transport;
  if (transportKind === 'stdio') {
    transport = new StdioClientTransport({ command: exec[0], args: exec.slice(1), cwd, env: childEnv, stderr: 'ignore' });
  } else {
    server = await startHttpServer();
    const url = new URL(`${server.base}/mcp`);
    const requestInit = { headers: { Authorization: `Bearer ${server.token}` } };
    if (transportKind === 'sse') {
      transport = new SSEClientTransport(url, { requestInit });
    } else if (transportKind === 'streamable') {
      transport = new StreamableHTTPClientTransport(url, { requestInit });
    } else {
      throw new Error(`unknown transport ${transportKind}`);
    }
  }

  const client = new Client({ name: 's1-harness', version: '0.0.0' });
  try {
    await client.connect(transport);
    const version = client.getServerVersion();
    check('initialize', version?.name === 'happy-platform-mcp', `${version?.name}@${version?.version}`);
    const negotiated = transport.protocolVersion ?? transport._protocolVersion ?? 'n/a';
    check('negotiated protocol recorded', true, String(negotiated));
    if (transportKind === 'streamable') {
      check('Mcp-Session-Id assigned', Boolean(transport.sessionId), transport.sessionId ? 'present' : 'absent');
    }

    const list = await client.listTools();
    check('tools/list returns 60 tools', list.tools.length === 60, `count=${list.tools.length}`);
    const normalized = normalizeToolsList({ tools: list.tools });
    writeFileSync(path.join(outDir, `tools-list.${label}.normalized.json`), normalized);
    check('tools/list normalized == fixture (byte-equal)', normalized === fixture,
      normalized === fixture ? 'identical' : `differs (len ${normalized.length} vs ${fixture.length})`);

    const call = await client.callTool({
      name: 'SN-Query-Table',
      arguments: { table_name: 'sys_user', limit: 1, fields: 'sys_id,user_name' }
    });
    const text = call.content?.[0]?.text ?? '';
    writeFileSync(path.join(outDir, `sn-query-table.${label}.json`), `${JSON.stringify(call, null, 2)}\n`);
    const prefix = 'Found 1 records in sys_user:\n';
    let rows = null;
    try { rows = JSON.parse(text.slice(prefix.length)); } catch { /* checked below */ }
    check('SN-Query-Table text format', text.startsWith(prefix) && call.isError !== true, text.split('\n')[0]);
    check('SN-Query-Table rows have sys_id,user_name only',
      Array.isArray(rows) && rows.length === 1 && Object.keys(rows[0]).join(',') === 'sys_id,user_name',
      rows ? `keys=${Object.keys(rows[0] ?? {}).join(',')}` : 'unparsed');
    check('SN-Query-Table pretty JSON (2-space)', rows !== null && text.slice(prefix.length) === JSON.stringify(rows, null, 2));

    let unknown;
    try {
      unknown = await client.callTool({ name: 'SN-Does-Not-Exist', arguments: {} });
      unknown = { result: unknown };
    } catch (error) {
      unknown = { error: { code: error.code, message: error.message } };
    }
    check('unknown tool observed', true, JSON.stringify(unknown).slice(0, 160));
  } catch (error) {
    check('harness run', false, error?.stack ?? String(error));
  } finally {
    await client.close().catch(() => {});
    if (server) {
      server.child.kill('SIGTERM');
      writeFileSync(path.join(outDir, `server-stderr.${label}.log`), server.stderr());
    }
  }
  const passed = results.filter((r) => r.name !== 'unknown tool observed' && r.name !== 'negotiated protocol recorded')
    .every((r) => r.ok);
  const summary = { label, transport: transportKind, exec, passed, results };
  writeFileSync(path.join(outDir, `harness.${label}.json`), `${JSON.stringify(summary, null, 2)}\n`);
  console.log(JSON.stringify(summary, null, 2));
  process.exit(passed ? 0 : 1);
}

main();
