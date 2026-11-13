// Stage: MCP protocol surface of the JS stdio server (6.2.0).
//   initialize (several client protocol versions), capabilities, tools/list, resources/*,
//   prompts/*, ping, protocol-level error shapes, startup banner, in both
//   full mode (stub ServiceNow) and docs-only mode.
// Wire bytes are captured raw (no SDK) so response shapes are exactly what the server wrote.
import path from 'node:path';
import { spawnMcp, PROTOCOL_VERSION } from '../lib/rpc.mjs';
import { CLI_JS } from '../lib/paths.mjs';
import { makeSandbox, refreshScrubber } from '../lib/world.mjs';

const sleep = ms => new Promise(r => setTimeout(r, ms));
const strip = ({ jsonrpc, id, ...rest }) => rest; // eslint-disable-line no-unused-vars

async function session(world, name, { env, args = [], docsOnly = false, noCreds = false, protocolVersion = PROTOCOL_VERSION, steps, rawSteps = [] }) {
  const box = makeSandbox(world, `surface-${name}`, { env: docsOnly ? { HAPPY_MCP_DOCS_ONLY: 'true' } : {}, withCreds: !(docsOnly || noCreds) });
  if (env) Object.assign(box.env, env);
  world.stub.reset(null);
  const server = spawnMcp({ script: CLI_JS, args, env: box.env, cwd: box.work });
  const record = { id: `surface/${name}`, mode: docsOnly || noCreds ? 'docs-only' : 'full', clientProtocolVersion: protocolVersion, exchanges: [], harnessError: null };
  try {
    const init = await server.request('initialize', { protocolVersion, capabilities: {}, clientInfo: { name: 'golden-capture', version: '0.0.0' } });
    record.exchanges.push({ request: { method: 'initialize', params: { protocolVersion, capabilities: {}, clientInfo: { name: 'golden-capture', version: '0.0.0' } } }, response: strip(init.message), notifications: init.notifications });
    server.notify('notifications/initialized');
    await sleep(100);
    for (const [method, params] of steps) {
      world.stub.take();
      const { message, notifications } = await server.request(method, params, { timeoutMs: 20000 });
      await sleep(30);
      record.exchanges.push({
        request: { method, ...(params === undefined ? {} : { params }) },
        response: strip(message),
        notifications,
        outbound: world.stub.take().map(r => ({ target: 'servicenow', ...r }))
      });
    }
    record.wireExchanges = [];
    for (const raw of rawSteps) {
      const mark = server.notifications.length;
      server.child.stdin.write(`${raw.message}\n`);
      await sleep(raw.waitMs ?? 300);
      record.wireExchanges.push({ sentRaw: raw.message, received: server.notifications.slice(mark) });
    }
  } catch (error) {
    record.harnessError = error.message;
  }
  const closed = await server.close();
  record.exit = { code: closed.code, signal: closed.signal };
  record.stderr = closed.stderr.split('\n').filter(Boolean);
  record.strayStdout = server.orphans.slice();
  return record;
}

export async function runSurface(world) {
  const out = new Map();
  const put = (file, doc) => out.set(path.posix.join('surface', file), doc);
  const sessions = [];

  // Full mode against the stub instance, default client protocol version.
  sessions.push(['full-default', await session(world, 'full-default', { steps: [
    ['tools/list'],
    ['tools/list', { cursor: 'golden-cursor' }],
    ['resources/list'],
    ['resources/templates/list'],
    ['resources/read', { uri: 'servicenow://instance' }],
    ['resources/read', { uri: 'servicenow://tables/all' }],
    ['resources/read', { uri: 'servicenow://unknown' }],
    ['prompts/list'],
    ['prompts/get', { name: 'nope' }],
    ['ping'],
    ['logging/setLevel', { level: 'info' }],
    ['completion/complete', { ref: { type: 'ref/prompt', name: 'x' }, argument: { name: 'a', value: '' } }],
    ['no/such/method'],
    ['tools/call', { name: 'SN-Does-Not-Exist', arguments: {} }],
    ['tools/call', { name: 'SN-Query-Table', arguments: {} }],
    ['tools/call', { name: 'SN-Query-Table', arguments: { table_name: 'incident', limit: 'not-a-number' } }],
    ['tools/call', {}]
  ] })]);

  // Protocol version negotiation (legacy + target + unknown).
  for (const version of ['2024-11-05', '2025-03-26', '2025-11-25', '2026-07-28', '1999-01-01']) {
    sessions.push([`init-${version}`, await session(world, `init-${version}`, { protocolVersion: version, steps: [['tools/list'], ['ping']] })]);
  }

  // Docs-only mode (no credentials).
  sessions.push(['docs-only', await session(world, 'docs-only', { docsOnly: true, steps: [
    ['tools/list'],
    ['resources/list'],
    ['resources/read', { uri: 'servicenow://instance' }],
    ['tools/call', { name: 'SN-Query-Table', arguments: { table_name: 'incident' } }],
    ['tools/call', { name: 'SN-Docs-Families', arguments: {} }]
  ] })]);

  sessions.push(['docs-only-flag', await session(world, 'docs-only-flag', { noCreds: true, args: ['--docs-only'], steps: [['tools/list']] })]);

  // Malformed / unusual wire input (responses arrive as notifications with id null).
  sessions.push(['wire-errors', await session(world, 'wire-errors', { steps: [], rawSteps: [
    { message: '{"jsonrpc":"2.0","id":901,"method":"tools/list"' + '' },
    { message: '[]' },
    { message: '{"jsonrpc":"2.0","id":902}' },
    { message: '{"jsonrpc":"1.0","id":903,"method":"ping"}' },
    { message: '{"jsonrpc":"2.0","id":904,"method":"ping"}' }
  ] })]);

  for (const [name, record] of sessions) {
    // wire-errors: unmatched replies land in the next exchange's notification list; surface them on the record.
    refreshScrubber(world);
    put(`${name}.json`, world.scrubber.deep(record));
  }
  return out;
}
