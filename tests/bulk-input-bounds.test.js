/**
 * Bulk/workflow/update-set input bounds (issue #67: VULN-012, VULN-032..035).
 *
 * Every collection is validated as a whole before the first ServiceNow write;
 * update-set ID lookups are chunked and paginated so no record is silently
 * dropped; batch placeholders resolve in one pass per operation.
 */
import http from 'node:http';
import { afterEach, describe, expect, jest, test } from '@jest/globals';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { ServiceNowClient } from '../src/servicenow-client.js';
import { createMcpServer } from '../src/mcp-server-consolidated.js';

const servers = [];

afterEach(async () => {
  jest.restoreAllMocks();
  await Promise.all(servers.splice(0).map(server => new Promise(resolve => {
    server.closeAllConnections?.();
    server.close(resolve);
  })));
});

const hex = n => n.toString(16).padStart(32, '0');

/**
 * Minimal fake ServiceNow Table API. Counts every request; GET supports
 * sys_idIN lookups and `sys_id>cursor` keyset pages (records are kept in
 * sys_id order) with sysparm_limit/sysparm_offset.
 */
async function fakeServiceNow({ records = [], getRecord } = {}) {
  const state = { requests: [], writes: [], gets: [], nextId: 1 };
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', chunk => { body += chunk; });
    req.on('end', () => {
      const url = new URL(req.url, 'http://fake');
      const entry = { method: req.method, path: url.pathname, query: url.searchParams, body: body ? JSON.parse(body) : undefined };
      state.requests.push(entry);
      const send = result => {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ result }));
      };
      const segments = url.pathname.split('/').filter(Boolean); // api now table <table> [id]
      if (req.method === 'GET') {
        state.gets.push(entry);
        if (segments.length === 5) return send(getRecord ? getRecord(segments[3], segments[4]) : { sys_id: segments[4], name: 'source' });
        const query = url.searchParams.get('sysparm_query') || '';
        const limit = Number(url.searchParams.get('sysparm_limit') || 10000);
        const offset = Number(url.searchParams.get('sysparm_offset') || 0);
        let matched = records;
        const inMatch = /^sys_idIN([0-9a-f,]*)/.exec(query);
        if (inMatch) {
          const wanted = new Set(inMatch[1].split(','));
          matched = records.filter(record => wanted.has(record.sys_id));
        }
        const cursor = /(?:^|\^)sys_id>([0-9a-f]{32})(?:\^|$)/.exec(query);
        if (cursor) matched = matched.filter(record => record.sys_id > cursor[1]);
        return send(matched.slice(offset, offset + limit));
      }
      state.writes.push(entry);
      if (req.method === 'POST') return send({ sys_id: hex(state.nextId++), ...entry.body });
      return send({ sys_id: segments[4], ...entry.body });
    });
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  servers.push(server);
  const url = `http://127.0.0.1:${server.address().port}`;
  return { state, url, client: new ServiceNowClient(url, 'admin', 'unit-test-non-secret') };
}

const createOps = (count, data = () => ({ short_description: 'x' })) =>
  Array.from({ length: count }, (_, i) => ({ table: 'incident', data: data(i) }));
const updateOps = count =>
  Array.from({ length: count }, (_, i) => ({ table: 'incident', sys_id: hex(i + 1), data: { state: 6 } }));
const updateRecords = count =>
  Array.from({ length: count }, (_, i) => ({ sys_id: hex(i + 1), name: `rec${i}`, type: 'Business Rule' }));

describe('SN-Batch-Create runtime bounds', () => {
  test.each([
    ['101 operations', createOps(101)],
    ['an operation whose data exceeds 64 KiB', [...createOps(50), { table: 'incident', data: { blob: 'x'.repeat(64 * 1024) } }, ...createOps(10)]],
    ['an invalid table name in the last operation', [...createOps(99), { table: '../sys_user', data: {} }]],
    ['a non-object data payload', [...createOps(5), { table: 'incident', data: 'nope' }]],
    ['an invalid save_as name', [{ table: 'incident', data: {}, save_as: 'bad name}' }]]
  ])('rejects %s with zero writes', async (_label, operations) => {
    const { state, client } = await fakeServiceNow();
    await expect(client.batchCreate(operations, false, false)).rejects.toThrow();
    expect(state.requests).toHaveLength(0);
  });

  test('100 operations at the 64 KiB data boundary complete', async () => {
    const { state, client } = await fakeServiceNow();
    const filler = 'y'.repeat(64 * 1024 - JSON.stringify({ blob: '' }).length);
    const operations = createOps(100, () => ({ blob: filler }));
    expect(Buffer.byteLength(JSON.stringify(operations[0].data))).toBe(64 * 1024);
    const result = await client.batchCreate(operations, true, false);
    expect(result.created_count).toBe(100);
    expect(state.writes).toHaveLength(100);
  });

  test('resolves references to previously saved IDs and preserves literal values', async () => {
    const { state, client } = await fakeServiceNow();
    const result = await client.batchCreate([
      { table: 'incident', data: { short_description: 'parent' }, save_as: 'parent' },
      { table: 'incident', data: { short_description: 'sibling' } },
      {
        table: 'incident',
        data: {
          parent: '${parent}',
          sibling: '${operation_1}',
          both: 'p=${parent};p=${parent};s=${operation_1}',
          future: '${child}',
          unknown: '${not_saved}',
          malformed: '${parent',
          spaced: '${ parent }',
          dollar: '$${parent}}',
          quote: 'say "${parent}"',
          nested: { list: ['${parent}', 7, null, true] },
          number: 42
        },
        save_as: 'child'
      }
    ], true, false);

    expect(result.success).toBe(true);
    const parentId = result.sys_ids.parent;
    const siblingId = result.sys_ids.operation_1;
    expect(parentId).toBe(hex(1));
    expect(siblingId).toBe(hex(2));
    expect(state.writes[2].body).toEqual({
      parent: parentId,
      sibling: siblingId,
      both: `p=${parentId};p=${parentId};s=${siblingId}`,
      future: '${child}',
      unknown: '${not_saved}',
      malformed: '${parent',
      spaced: '${ parent }',
      dollar: `$${parentId}}`,
      quote: `say "${parentId}"`,
      nested: { list: [parentId, 7, null, true] },
      number: 42
    });
  });

  test.each([
    ['__proto__', [{ table: 'incident', data: {}, save_as: '__proto__' }]],
    ['constructor', [{ table: 'incident', data: {}, save_as: 'constructor' }]],
    ['prototype', [{ table: 'incident', data: {}, save_as: 'prototype' }]],
    ['a duplicate save_as', [{ table: 'incident', data: {}, save_as: 'a' }, { table: 'incident', data: {}, save_as: 'a' }]],
    ['a save_as colliding with a default operation_N key', [{ table: 'incident', data: {} }, { table: 'incident', data: {}, save_as: 'operation_0' }]],
    ['a reserved operation_N save_as on its own index', [{ table: 'incident', data: {}, save_as: 'operation_0' }]]
  ])('rejects save_as %s with zero writes', async (_label, operations) => {
    const { state, client } = await fakeServiceNow();
    await expect(client.batchCreate(operations, true, false)).rejects.toThrow(/save_as/);
    expect(state.requests).toHaveLength(0);
  });

  test('saved IDs live in a null-prototype map so inherited names never resolve', async () => {
    const { state, client } = await fakeServiceNow();
    const result = await client.batchCreate([
      { table: 'incident', data: {}, save_as: 'toString' },
      { table: 'incident', data: { a: '${toString}', b: '${hasOwnProperty}', c: '${valueOf}' } }
    ], true, false);
    expect(Object.getPrototypeOf(result.sys_ids)).toBeNull();
    expect(result.sys_ids.toString).toBe(hex(1));
    expect(state.writes[1].body).toEqual({ a: hex(1), b: '${hasOwnProperty}', c: '${valueOf}' });
  });

  test('transaction mode stops at the first failed write after validation', async () => {
    const { state, client } = await fakeServiceNow();
    let calls = 0;
    const original = client.createRecord.bind(client);
    client.createRecord = async (table, data) => {
      calls += 1;
      if (calls === 2) throw new Error('boom');
      return original(table, data);
    };
    await expect(client.batchCreate(createOps(5), true, false)).rejects.toThrow(/operation 1/);
    expect(state.writes).toHaveLength(1);
  });

  test('placeholder-dense operations resolve correctly and scale linearly', async () => {
    // Each op references every earlier saved ID many times, padded to the same
    // ~50 KiB per operation, so total input grows linearly with N.
    const placeholderOps = count => Array.from({ length: count }, (_, i) => {
      const refs = [];
      for (let k = 0; k < i; k += 1) refs.push(`\${k${k}}`);
      const unit = refs.length ? refs.join(',') : 'none';
      const repeat = Math.max(1, Math.floor((50 * 1024) / (unit.length + 1)));
      return { table: 'incident', save_as: `k${i}`, data: { refs: Array(repeat).fill(unit).join('|'), stray: '${'.repeat(5000) } };
    });
    const run = async count => {
      const client = new ServiceNowClient('https://dev.service-now.com', 'admin', 'pw');
      let next = 0;
      const posted = [];
      client.createRecord = async (_table, data) => {
        posted.push(data);
        next += 1;
        return { sys_id: hex(next) };
      };
      const operations = placeholderOps(count);
      const start = performance.now();
      const result = await client.batchCreate(operations, true, false);
      return { elapsed: performance.now() - start, result, posted };
    };
    const median = values => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)];
    const sample = async count => {
      const runs = [];
      for (let r = 0; r < 3; r += 1) runs.push(await run(count));
      return { runs, time: median(runs.map(entry => entry.elapsed)) };
    };

    await run(50); // warm-up
    const small = await sample(50);
    const large = await sample(100);

    const { result, posted } = large.runs[0];
    expect(result.created_count).toBe(100);
    expect(posted[99].refs.startsWith(`${hex(1)},${hex(2)}`)).toBe(true);
    expect(posted[99].refs).not.toContain('${k');
    expect(posted[99].stray).toBe('${'.repeat(5000));
    // Linear work doubles (~2x); a quadratic rescan would quadruple (~4x).
    expect(large.time / small.time).toBeLessThan(3);
    // Catastrophic guard only; not a performance budget.
    expect(large.time).toBeLessThan(15_000);
  }, 120_000);
});

describe('SN-Batch-Update runtime bounds', () => {
  test.each([
    ['101 updates', updateOps(101)],
    ['a non-hex sys_id', [...updateOps(10), { table: 'incident', sys_id: 'abc', data: {} }]],
    ['an uppercase sys_id', [{ table: 'incident', sys_id: hex(1).toUpperCase().replace(/0/g, 'A'), data: {} }]],
    ['oversized data', [...updateOps(3), { table: 'incident', sys_id: hex(9), data: { blob: 'x'.repeat(64 * 1024) } }]],
    ['an invalid table', [{ table: 'incident/../../x', sys_id: hex(1), data: {} }]]
  ])('rejects %s with zero writes', async (_label, updates) => {
    const { state, client } = await fakeServiceNow();
    await expect(client.batchUpdate(updates, false, false)).rejects.toThrow();
    expect(state.requests).toHaveLength(0);
  });

  test('100 updates complete', async () => {
    const { state, client } = await fakeServiceNow();
    const result = await client.batchUpdate(updateOps(100), false, false);
    expect(result.updated_count).toBe(100);
    expect(state.writes).toHaveLength(100);
  });
});

describe('SN-Create-Workflow runtime bounds', () => {
  const activities = n => Array.from({ length: n }, (_, i) => ({ name: `A${i}`, script: 'gs.info(1);' }));
  const transitions = (n, max) => Array.from({ length: n }, (_, i) => ({ from: i % max, to: (i + 1) % max }));
  const spec = extra => ({ name: 'WF', table: 'incident', activities: activities(3), transitions: transitions(2, 3), ...extra });

  test.each([
    ['101 activities', spec({ activities: activities(101) })],
    ['201 transitions', spec({ activities: activities(100), transitions: transitions(201, 100) })],
    ['a script over 256 KiB', spec({ activities: [{ name: 'big', script: 's'.repeat(256 * 1024 + 1) }] })],
    ['a transition condition over 256 KiB', spec({ transitions: [{ from: 0, to: 1, condition: 'c'.repeat(256 * 1024 + 1) }] })],
    ['total input over 4 MiB', spec({ activities: Array.from({ length: 17 }, (_, i) => ({ name: `A${i}`, script: 's'.repeat(250 * 1024) })) })],
    ['an overlong workflow name', spec({ name: 'n'.repeat(256) })],
    ['an overlong activity name', spec({ activities: [{ name: 'n'.repeat(256) }] })],
    ['an out-of-range transition index', spec({ transitions: [{ from: 0, to: 3 }] })],
    ['an invalid table', spec({ table: 'incident;drop' })],
    ['an invalid activity definition sys_id', spec({ activities: [{ name: 'a', activity_definition_sys_id: 'nope' }] })]
  ])('rejects %s with zero writes', async (_label, workflow) => {
    const { state, client } = await fakeServiceNow();
    await expect(client.createCompleteWorkflow(workflow, false)).rejects.toThrow();
    expect(state.requests).toHaveLength(0);
  });

  test('100 activities and 200 transitions at the script boundary complete', async () => {
    const { state, client } = await fakeServiceNow();
    const acts = activities(100);
    acts[0].script = 's'.repeat(256 * 1024);
    const result = await client.createCompleteWorkflow({
      name: 'n'.repeat(255),
      table: 'incident',
      activities: acts,
      transitions: transitions(200, 100),
      publish: true
    }, false);
    expect(Object.keys(result.activity_sys_ids)).toHaveLength(100);
    expect(result.transition_sys_ids).toHaveLength(200);
    // workflow + version + 100 activities + 200 transitions + publish
    expect(state.writes).toHaveLength(303);
  });

  test.each([
    ['an undeclared string transition endpoint', spec({ transitions: [{ from: 'Nope', to: 1 }] })],
    ['an inherited-property endpoint', spec({ transitions: [{ from: '__proto__', to: 1 }] })],
    ['an undeclared default activity key', spec({ transitions: [{ from: 'activity_0', to: 1 }] })],
    ['an undeclared start_activity', spec({ start_activity: 'nope', publish: true })],
    ['duplicate activity ids', spec({ activities: [{ id: 'x', name: 'A' }, { id: 'x', name: 'B' }], transitions: [] })],
    ['an ambiguous activity name reference', spec({ activities: [{ name: 'Same' }, { name: 'Same' }], transitions: [{ from: 'Same', to: 0 }] })]
  ])('rejects %s with zero writes', async (_label, workflow) => {
    const { state, client } = await fakeServiceNow();
    await expect(client.createCompleteWorkflow(workflow, false)).rejects.toThrow(/activit/);
    expect(state.requests).toHaveLength(0);
  });

  test('resolves declared ids, unique names and external sys_ids before writing transitions', async () => {
    const { state, client } = await fakeServiceNow();
    const external = hex(0xe0);
    const result = await client.createCompleteWorkflow({
      name: 'WF',
      table: 'change_request',
      activities: [{ id: 'check', name: 'Check Risk' }, { name: 'Auto Approve' }],
      transitions: [
        { from: 'check', to: 'Auto Approve' },
        { from: 'Auto Approve', to: external },
        { from: 1, to: 0 }
      ],
      start_activity: 'Auto Approve',
      publish: true
    }, false);
    // wf=1, version=2, activities=3,4, transitions=5..7
    expect(Object.getPrototypeOf(result.activity_sys_ids)).toBeNull();
    expect(result.activity_sys_ids.check).toBe(hex(3));
    const transitionBodies = state.writes.filter(write => write.path.endsWith('/wf_transition')).map(write => [write.body.from, write.body.to]);
    expect(transitionBodies).toEqual([[hex(3), hex(4)], [hex(4), external], [hex(4), hex(3)]]);
    expect(state.writes.at(-1).body.start).toBe(hex(4));
    expect(result.start_activity).toBe(hex(4));
  });
});

describe('SN-Move-Records-To-Update-Set bounds and completeness', () => {
  const target = hex(0xabc);

  test.each([
    ['201 record IDs', { record_sys_ids: updateRecords(201).map(r => r.sys_id) }],
    ['an invalid record ID', { record_sys_ids: [hex(1), 'not-a-sys-id'] }],
    ['an encoded-query injection in record IDs', { record_sys_ids: [`${hex(1)}^ORsys_id!=x`] }],
    ['a malformed time range', { time_range: { start: '2025-01-01 00:00:00^ORactive=true', end: '2025-01-02 00:00:00' } }],
    ['a source update set containing ^', { time_range: { start: '2025-01-01 00:00:00', end: '2025-01-02 00:00:00' }, source_update_set: 'Default^ORname!=x' }],
    ['an invalid table', { record_sys_ids: [hex(1)], table: 'sys_update_xml/../sys_user' }]
  ])('rejects %s before any request', async (_label, options) => {
    const { state, client } = await fakeServiceNow({ records: updateRecords(5) });
    await expect(client.moveRecordsToUpdateSet(target, { ...options, reportProgress: false })).rejects.toThrow();
    expect(state.requests).toHaveLength(0);
  });

  test('rejects an invalid target update set ID before any request', async () => {
    const { state, client } = await fakeServiceNow({ records: updateRecords(5) });
    await expect(client.moveRecordsToUpdateSet('Default', { record_sys_ids: [hex(1)], reportProgress: false })).rejects.toThrow();
    expect(state.requests).toHaveLength(0);
  });

  test('200 IDs are fetched in bounded chunks of at most 100 and all moved', async () => {
    const records = updateRecords(200);
    const { state, client } = await fakeServiceNow({ records });
    const result = await client.moveRecordsToUpdateSet(target, { record_sys_ids: records.map(r => r.sys_id), reportProgress: false });
    expect(result.moved).toBe(200);
    expect(state.gets).toHaveLength(2);
    for (const get of state.gets) {
      const query = get.query.get('sysparm_query');
      expect(query.length).toBeLessThanOrEqual(4096);
      expect(query.slice('sys_idIN'.length).split(',').length).toBeLessThanOrEqual(100);
    }
    expect(state.writes).toHaveLength(200);
    expect(state.writes.every(write => write.body.update_set === target)).toBe(true);
  });

  test('time-range moves collect more than 1000 matches without silent truncation', async () => {
    const records = updateRecords(2500);
    const { state, client } = await fakeServiceNow({ records });
    const result = await client.moveRecordsToUpdateSet(target, {
      time_range: { start: '2025-01-01 00:00:00', end: '2025-01-02 00:00:00' },
      source_update_set: 'Default',
      reportProgress: false
    });
    expect(result.moved).toBe(2500);
    expect(new Set(state.writes.map(write => write.path)).size).toBe(2500);
    expect(state.gets).toHaveLength(3);
    // Keyset pagination: no offsets; each later page starts after the previous page's last sys_id.
    expect(state.gets.every(get => !get.query.has('sysparm_offset'))).toBe(true);
    expect(state.gets[0].query.get('sysparm_query')).not.toContain('sys_id>');
    expect(state.gets[1].query.get('sysparm_query')).toContain(`^sys_id>${hex(1000)}^ORDERBYsys_id`);
    expect(state.gets[2].query.get('sysparm_query')).toContain(`^sys_id>${hex(2000)}^ORDERBYsys_id`);
  }, 20_000);

  test('a time-range match above the move cap fails before any write', async () => {
    const { state, client } = await fakeServiceNow({ records: updateRecords(10_001) });
    await expect(client.moveRecordsToUpdateSet(target, {
      time_range: { start: '2025-01-01 00:00:00', end: '2025-01-02 00:00:00' },
      reportProgress: false
    })).rejects.toThrow(/10000/);
    expect(state.writes).toHaveLength(0);
  });
});

describe('SN-Clone-Update-Set bounds and completeness', () => {
  test('rejects invalid source IDs and names before any request', async () => {
    const { state, client } = await fakeServiceNow();
    await expect(client.cloneUpdateSet('Default', 'copy', false)).rejects.toThrow();
    await expect(client.cloneUpdateSet(hex(1), 'n'.repeat(256), false)).rejects.toThrow();
    await expect(client.cloneUpdateSet(hex(1), '', false)).rejects.toThrow();
    expect(state.requests).toHaveLength(0);
  });

  test('clones every source record beyond a single page and creates the set after fetching', async () => {
    const client = new ServiceNowClient('https://dev.service-now.com', 'admin', 'pw');
    const source = Array.from({ length: 6000 }, (_, i) => ({ sys_id: hex(i + 1), name: `r${i}`, type: 't', payload: '<x/>' }));
    const events = [];
    client.getRecord = async () => ({ sys_id: hex(0xfff), name: 'Source' });
    client.getRecords = async (_table, query) => {
      events.push('get');
      const cursor = /\^sys_id>([0-9a-f]{32})\^/.exec(query.sysparm_query);
      expect(query.sysparm_offset).toBeUndefined();
      return source.filter(record => !cursor || record.sys_id > cursor[1]).slice(0, Number(query.sysparm_limit));
    };
    let created = 0;
    client.createRecord = async (table, data) => {
      events.push(table);
      created += 1;
      return { sys_id: hex(100000 + created), ...data };
    };
    const result = await client.cloneUpdateSet(hex(0xfff), 'Copy', false);
    expect(result.records_cloned).toBe(6000);
    expect(result.total_source_records).toBe(6000);
    expect(events.indexOf('sys_update_set')).toBeGreaterThan(events.lastIndexOf('get'));
  });
});

describe('MCP tool schemas advertise the limits', () => {
  test('bulk, workflow, update-set and NL inputs carry explicit bounds', async () => {
    const server = await createMcpServer({ setProgressCallback() {} });
    const { tools } = await server._requestHandlers.get('tools/list')({ method: 'tools/list', params: {} }, {});
    const schema = name => tools.find(tool => tool.name === name).inputSchema.properties;

    expect(schema('SN-Batch-Create').operations.maxItems).toBe(100);
    expect(schema('SN-Batch-Create').operations.items.properties.save_as.pattern).toBeDefined();
    expect(schema('SN-Batch-Update').updates.maxItems).toBe(100);
    expect(schema('SN-Batch-Update').updates.items.properties.sys_id.pattern).toBe('^[0-9a-f]{32}$');
    expect(schema('SN-Create-Workflow').activities.maxItems).toBe(100);
    expect(schema('SN-Create-Workflow').transitions.maxItems).toBe(200);
    expect(schema('SN-Create-Workflow').activities.items.properties.script.maxLength).toBe(256 * 1024);
    expect(schema('SN-Create-Workflow').name.maxLength).toBe(255);
    expect(schema('SN-Move-Records-To-Update-Set').record_sys_ids.maxItems).toBe(200);
    expect(schema('SN-Move-Records-To-Update-Set').record_sys_ids.items.pattern).toBe('^[0-9a-f]{32}$');
    expect(schema('SN-Move-Records-To-Update-Set').update_set_id.pattern).toBe('^[0-9a-f]{32}$');
    expect(schema('SN-Clone-Update-Set').new_name.maxLength).toBe(255);
    expect(schema('SN-Natural-Language-Search').query.maxLength).toBe(2048);
  });

  test('NL search never logs an over-limit query', async () => {
    const lines = [];
    jest.spyOn(console, 'error').mockImplementation((...args) => lines.push(args.join(' ')));
    const client = { setProgressCallback() {}, getRecords: async () => [] };
    const server = await createMcpServer(client);
    const query = `priority ${'z'.repeat(100_000)}`;
    await server._requestHandlers.get('tools/call')({
      method: 'tools/call',
      params: { name: 'SN-Natural-Language-Search', arguments: { query } }
    }, {});
    expect(lines.length).toBeGreaterThan(0);
    for (const line of lines) {
      expect(line.length).toBeLessThanOrEqual(2048);
      expect(line).not.toContain('z'.repeat(2049));
    }
  });
});

describe('real MCP dispatch against a fake ServiceNow endpoint', () => {
  test('batch create resolves placeholders and an over-limit batch performs zero writes', async () => {
    const { state, client } = await fakeServiceNow();
    const server = await createMcpServer(client);
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    const mcp = new Client({ name: 'bulk-bounds-smoke', version: '1.0.0' });
    await server.connect(serverTransport);
    await mcp.connect(clientTransport);
    try {
      const ok = await mcp.callTool({
        name: 'SN-Batch-Create',
        arguments: {
          operations: [
            { table: 'incident', data: { short_description: 'parent' }, save_as: 'parent' },
            { table: 'incident', data: { parent: '${parent}', note: 'literal ${missing}' } }
          ],
          progress: false
        }
      });
      expect(ok.isError).toBeFalsy();
      expect(ok.content[0].text).toContain('Batch create completed');
      expect(state.writes).toHaveLength(2);
      expect(state.writes[1].body).toEqual({ parent: hex(1), note: 'literal ${missing}' });

      const writesBefore = state.writes.length;
      const rejected = await mcp.callTool({
        name: 'SN-Batch-Create',
        arguments: { operations: createOps(101), progress: false }
      });
      expect(rejected.isError).toBe(true);
      expect(rejected.content[0].text).toMatch(/100/);
      expect(state.writes).toHaveLength(writesBefore);
    } finally {
      await mcp.close();
      await server.close();
    }
  });
});
