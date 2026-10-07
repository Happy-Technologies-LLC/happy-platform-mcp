// Update-set / application pickers and batch failures against a local fake
// ServiceNow. The fake models the real UI picker: state lives in a cookie
// session, a request without the session cookie lands in a fresh session, and
// the legacy preferences endpoint answers HTTP 400 (observed on live instances).
import http from 'node:http';
import { afterEach, describe, expect, jest, test } from '@jest/globals';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { ServiceNowClient } from '../src/servicenow-client.js';
import { createMcpServer } from '../src/mcp-server-consolidated.js';

const SET_A = 'a'.repeat(32);
const SET_DEFAULT = 'd'.repeat(32);
const APP_X = 'b'.repeat(32);

async function fakeServiceNow({ ignorePickerWrites = false, failPickerWrites = false, batchFailAt = null } = {}) {
  const sessions = new Map();
  const requests = [];
  let nextSession = 0;
  let created = 0;
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (chunk) => { body += chunk; });
    req.on('end', () => {
      const url = new URL(req.url, 'http://fake');
      requests.push({ method: req.method, path: url.pathname, cookie: req.headers.cookie || null });
      const json = (status, payload, headers = {}) => {
        res.writeHead(status, { 'content-type': 'application/json', ...headers });
        res.end(JSON.stringify(payload));
      };
      const sid = /JSESSIONID=([^;]+)/.exec(req.headers.cookie || '')?.[1];
      const session = (sid && sessions.get(sid)) || { updateSet: { sysId: SET_DEFAULT, name: 'Default' }, app: 'global' };

      if (req.method === 'GET' && url.pathname === '/') {
        const id = `s${++nextSession}`;
        sessions.set(id, { updateSet: { sysId: SET_DEFAULT, name: 'Default' }, app: 'global' });
        res.writeHead(200, { 'content-type': 'text/html', 'set-cookie': `JSESSIONID=${id}; Path=/; HttpOnly` });
        res.end('<html></html>');
        return;
      }
      if (url.pathname === '/api/now/ui/preferences/sys_update_set' || url.pathname === '/api/now/ui/preferences/apps.current') {
        json(400, { error: { message: 'Requested URI does not represent any resource' } });
        return;
      }
      if (url.pathname === '/api/now/ui/concoursepicker/updateset') {
        if (req.method === 'GET') return json(200, { result: { current: session.updateSet } });
        if (failPickerWrites) return json(500, { error: { message: 'picker unavailable' } });
        if (!ignorePickerWrites) {
          const { sysId, name } = JSON.parse(body);
          session.updateSet = { sysId, name };
        }
        return json(200, { result: { success: true } });
      }
      if (url.pathname === '/api/now/ui/concoursepicker/application') {
        if (req.method === 'GET') {
          return json(200, { result: { current: session.app, list: [{ sysId: 'global', name: 'Global' }, { sysId: APP_X, name: 'App X' }] } });
        }
        if (!ignorePickerWrites) session.app = JSON.parse(body).app_id;
        return json(200, { result: { app_id: session.app } });
      }
      if (req.method === 'GET' && url.pathname === `/api/now/table/sys_update_set/${SET_A}`) {
        return json(200, { result: { sys_id: SET_A, name: 'Story set' } });
      }
      if (req.method === 'GET' && url.pathname === '/api/now/table/sys_scope/global') {
        return json(200, { result: { sys_id: 'global', name: 'Global', scope: 'global' } });
      }
      if (req.method === 'GET' && url.pathname === `/api/now/table/sys_app/${APP_X}`) {
        return json(200, { result: { sys_id: APP_X, name: 'App X', scope: 'x_app' } });
      }
      if (req.method === 'POST' && url.pathname === '/api/now/table/incident') {
        created += 1;
        if (batchFailAt === created) return json(403, { error: { message: 'ACL denied' } });
        return json(201, { result: { sys_id: String(created).padStart(32, '0') } });
      }
      if (req.method === 'PATCH' || req.method === 'PUT') {
        if (url.pathname.startsWith('/api/now/table/incident/')) {
          return batchFailAt ? json(403, { error: { message: 'ACL denied' } }) : json(200, { result: { sys_id: url.pathname.split('/').pop() } });
        }
      }
      json(404, { error: { message: `unhandled ${req.method} ${url.pathname}` } });
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const url = `http://127.0.0.1:${server.address().port}`;
  return {
    url,
    requests,
    client: new ServiceNowClient(url, 'admin', 'inert-test-password'),
    close: () => new Promise((resolve) => { server.closeAllConnections?.(); server.close(resolve); })
  };
}

const open = [];
afterEach(async () => {
  jest.restoreAllMocks();
  for (const fake of open.splice(0)) await fake.close();
});
async function fake(options) {
  const f = await fakeServiceNow(options);
  open.push(f);
  return f;
}
async function mcpClient(serviceNowClient) {
  const server = await createMcpServer(serviceNowClient);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);
  const client = new Client({ name: 'session-picker-test', version: '1.0.0' });
  await client.connect(clientTransport);
  return client;
}
const textOf = (result) => result.content.map((c) => c.text).join('\n');

describe('current update set', () => {
  test('reads the picker instead of the preferences endpoint that answers 400', async () => {
    const f = await fake();
    const result = await f.client.getCurrentUpdateSet();
    expect(result.result).toEqual({ name: 'Default', value: SET_DEFAULT, sys_id: SET_DEFAULT });
    expect(f.requests.some((r) => r.path.startsWith('/api/now/ui/preferences/'))).toBe(false);
  });

  test('SN-Get-Current-Update-Set returns the current set through MCP', async () => {
    const f = await fake();
    const client = await mcpClient(f.client);
    const result = await client.callTool({ name: 'SN-Get-Current-Update-Set', arguments: {} });
    expect(result.isError).toBeFalsy();
    expect(textOf(result)).toContain(SET_DEFAULT);
  });
});

describe('set current update set', () => {
  test('keeps the session cookie across read, write and verification and reports the previous set', async () => {
    const f = await fake();
    const result = await f.client.setCurrentUpdateSet(SET_A);
    expect(result).toMatchObject({
      success: true, sys_id: SET_A, update_set: 'Story set', verified: true, method: 'ui_api',
      previous_update_set: { sys_id: SET_DEFAULT, name: 'Default' }
    });
    const pickerCalls = f.requests.filter((r) => r.path === '/api/now/ui/concoursepicker/updateset');
    expect(pickerCalls.map((r) => r.method)).toEqual(['GET', 'PUT', 'GET']);
    expect(new Set(pickerCalls.map((r) => r.cookie)).size).toBe(1);
    expect(pickerCalls[0].cookie).toMatch(/^JSESSIONID=s\d+$/);
  });

  test('fails instead of reporting success when the change does not take effect', async () => {
    const f = await fake({ ignorePickerWrites: true });
    await expect(f.client.setCurrentUpdateSet(SET_A)).rejects.toThrow(/verification failed/i);
  });

  test('never falls back to an unverified scheduled job', async () => {
    const f = await fake({ failPickerWrites: true });
    jest.spyOn(console, 'log').mockImplementation(() => {});
    await expect(f.client.setCurrentUpdateSet(SET_A)).rejects.toThrow();
    expect(f.requests.some((r) => r.path.startsWith('/api/now/table/sys_trigger'))).toBe(false);
  });

  test.each(['', 'not-a-sys-id', `${SET_A}x`, '../sys_user'])('rejects invalid sys_id %p before any request', async (id) => {
    const f = await fake();
    await expect(f.client.setCurrentUpdateSet(id)).rejects.toThrow(/sys_id/i);
    expect(f.requests).toHaveLength(0);
  });
});

describe('set current application', () => {
  test('accepts the platform Global identity and verifies it in the same session', async () => {
    const f = await fake();
    await f.client.setCurrentApplication(APP_X);
    const result = await f.client.setCurrentApplication('global');
    expect(result).toMatchObject({ success: true, application: 'Global', sys_id: 'global', verified: true });
    expect(f.requests.some((r) => r.path === '/api/now/table/sys_scope/global')).toBe(true);
    expect(f.requests.some((r) => r.path.startsWith('/api/now/ui/preferences/'))).toBe(false);
  });

  test('reports the previous scope from the picker', async () => {
    const f = await fake();
    const result = await f.client.setCurrentApplication(APP_X);
    expect(result).toMatchObject({ verified: true, previous_scope: { sys_id: 'global', name: 'Global' } });
  });

  test('SN-Set-Current-Application marks failures as MCP errors and shows verification on success', async () => {
    const f = await fake();
    jest.spyOn(console, 'error').mockImplementation(() => {});
    const client = await mcpClient(f.client);
    const ok = await client.callTool({ name: 'SN-Set-Current-Application', arguments: { app_sys_id: 'global' } });
    expect(ok.isError).toBeFalsy();
    expect(textOf(ok)).toMatch(/Verification: passed/);
    const bad = await client.callTool({ name: 'SN-Set-Current-Application', arguments: { app_sys_id: 'nope' } });
    expect(bad.isError).toBe(true);
  });
});

describe('batch tool failures', () => {
  test('SN-Batch-Create marks a failed batch as an MCP error', async () => {
    const f = await fake({ batchFailAt: 2 });
    jest.spyOn(console, 'error').mockImplementation(() => {});
    const client = await mcpClient(f.client);
    const ops = [{ table: 'incident', data: { short_description: 'one' } }, { table: 'incident', data: { short_description: 'two' } }];
    const failed = await client.callTool({ name: 'SN-Batch-Create', arguments: { operations: ops } });
    expect(failed.isError).toBe(true);
    const f2 = await fake();
    const ok = await (await mcpClient(f2.client)).callTool({ name: 'SN-Batch-Create', arguments: { operations: ops } });
    expect(ok.isError).toBeFalsy();
  });

  test('SN-Batch-Update marks a failed batch as an MCP error', async () => {
    const f = await fake({ batchFailAt: 1 });
    jest.spyOn(console, 'error').mockImplementation(() => {});
    const client = await mcpClient(f.client);
    const failed = await client.callTool({
      name: 'SN-Batch-Update',
      arguments: { updates: [{ table: 'incident', sys_id: '1'.repeat(32), data: { state: '2' } }] }
    });
    expect(failed.isError).toBe(true);
  });

  test('a batch that keeps going after failures still reports failure (create, transaction: false)', async () => {
    const f = await fake({ batchFailAt: 1 });
    const result = await f.client.batchCreate([{ table: 'incident', data: { a: 1 } }, { table: 'incident', data: { a: 2 } }], false, false);
    expect(result.created_count).toBe(1);
    expect(result.errors).toHaveLength(1);
    expect(result.success).toBe(false);
  });

  test('a batch that keeps going after failures still reports failure (update, stop_on_error: false)', async () => {
    const f = await fake({ batchFailAt: 1 });
    const result = await f.client.batchUpdate([{ table: 'incident', sys_id: '1'.repeat(32), data: { state: '2' } }], false, false);
    expect(result.errors).toHaveLength(1);
    expect(result.success).toBe(false);
  });
});
