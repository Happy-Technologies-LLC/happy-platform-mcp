#!/usr/bin/env node
// Raw-wire tools/list capture over stdio (no SDK parsing) and byte-level diff vs fixture.
//   node tools-diff.mjs --exec '["bin","stdio"]' --label rust [--cwd DIR] [--version 2025-11-25]
import { spawn, spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { normalizeToolsList } from './normalize.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const arg = (n, d) => { const i = process.argv.indexOf(`--${n}`); return i > 0 ? process.argv[i + 1] : d; };
const exec = JSON.parse(arg('exec'));
const label = arg('label');
const version = arg('version', '2025-11-25');
const outDir = arg('out', path.join(here, '..', 'out'));
mkdirSync(outDir, { recursive: true });
const fixturePath = path.join(here, '..', 'fixtures', 'tools-list-6.2.0.json');
const fixture = readFileSync(fixturePath, 'utf8');
const env = { ...process.env, SERVICENOW_INSTANCE: 'dev451274' };
for (const k of ['SERVICENOW_INSTANCE_URL', 'SERVICENOW_USERNAME', 'SERVICENOW_PASSWORD']) delete env[k];

const child = spawn(exec[0], exec.slice(1), { cwd: arg('cwd', process.cwd()), env, stdio: ['pipe', 'pipe', 'ignore'] });
let buf = '';
const waiters = new Map();
child.stdout.on('data', (d) => {
  buf += d;
  let nl;
  while ((nl = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, nl);
    buf = buf.slice(nl + 1);
    if (!line.trim()) continue;
    const msg = JSON.parse(line);
    if (msg.id !== undefined && waiters.has(msg.id)) { waiters.get(msg.id)(line); waiters.delete(msg.id); }
  }
});
const send = (msg) => child.stdin.write(`${JSON.stringify(msg)}\n`);
const request = (id, method, params) => new Promise((resolve) => { waiters.set(id, resolve); send({ jsonrpc: '2.0', id, method, params }); });

await request(1, 'initialize', { protocolVersion: version, capabilities: {}, clientInfo: { name: 'tools-diff', version: '0' } });
send({ jsonrpc: '2.0', method: 'notifications/initialized' });
const rawLine = await request(2, 'tools/list', {});
child.kill();

const result = JSON.parse(rawLine).result;
const wire = `${JSON.stringify(result, null, 2)}\n`;
const wirePath = path.join(outDir, `tools-list.${label}.wire.json`);
writeFileSync(wirePath, wire);
writeFileSync(path.join(outDir, `tools-list.${label}.wire-line.txt`), `${rawLine}\n`);
const normalized = normalizeToolsList(result);
const extraTopLevel = Object.keys(result).filter((k) => k !== 'tools');
const diffStat = (a) => {
  const r = spawnSync('diff', ['-u', fixturePath, a], { encoding: 'utf8' });
  return { identical: r.status === 0, changedLines: r.stdout.split('\n').filter((l) => /^[+-][^+-]/.test(l)).length };
};
const report = {
  label,
  toolCount: result.tools.length,
  topLevelKeys: Object.keys(result),
  extraTopLevelKeys: extraTopLevel,
  wireBytes: Buffer.byteLength(rawLine),
  wirePrettyVsFixture: diffStat(wirePath),
  normalizedVsFixtureByteEqual: normalized === fixture,
  wireToolOrder: result.tools.slice(0, 3).map((t) => t.name),
  wireFirstToolKeyOrder: Object.keys(result.tools[0])
};
writeFileSync(path.join(outDir, `tools-diff.${label}.json`), `${JSON.stringify(report, null, 2)}\n`);
console.log(JSON.stringify(report));
