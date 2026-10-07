/**
 * UI-client Axios error containment (issue #67: VULN-019).
 *
 * setCurrentApplication uses a separate cookie-bearing Axios client outside the
 * sanitizing interceptor. Its failures must surface only status/code/message.
 */
import http from 'node:http';
import { inspect } from 'node:util';
import axios from 'axios';
import { afterEach, describe, expect, jest, test } from '@jest/globals';
import { ServiceNowClient } from '../src/servicenow-client.js';
import { createMcpServer } from '../src/mcp-server-consolidated.js';

const PASSWORD = 'ui-client-password-fixture';
const USERNAME = 'ui-user';
const BASIC = Buffer.from(`${USERNAME}:${PASSWORD}`).toString('base64');
const APP_ID = 'a'.repeat(32);
const servers = [];

afterEach(async () => {
  jest.restoreAllMocks();
  await Promise.all(servers.splice(0).map(server => new Promise(resolve => server.close(resolve))));
});

/** Fake ServiceNow that fails the UI picker call while echoing the caller's Authorization header. */
async function fakeServiceNow() {
  const server = http.createServer((req, res) => {
    const json = (status, payload) => {
      res.writeHead(status, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify(payload));
    };
    if (req.method === 'GET' && req.url === '/') {
      res.writeHead(200, { 'Content-Type': 'text/html', 'Set-Cookie': 'JSESSIONID=session-cookie-fixture' });
      res.end('<html></html>');
      return;
    }
    if (req.method === 'GET' && req.url.startsWith('/api/now/ui/preferences/apps.current')) {
      json(200, { result: { value: 'b'.repeat(32), display_value: 'Global' } });
      return;
    }
    if (req.method === 'GET' && req.url.startsWith(`/api/now/table/sys_app/${APP_ID}`)) {
      json(200, { result: { sys_id: APP_ID, name: 'Fixture App', scope: 'x_fixture' } });
      return;
    }
    if (req.method === 'PUT' && req.url === '/api/now/ui/concoursepicker/application') {
      json(500, { error: { message: 'picker failed', detail: `echo ${req.headers.authorization}` } });
      return;
    }
    json(404, { error: { message: 'not found' } });
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  servers.push(server);
  return `http://127.0.0.1:${server.address().port}`;
}

function captureConsole() {
  const lines = [];
  for (const method of ['error', 'log', 'warn', 'info', 'debug']) {
    jest.spyOn(console, method).mockImplementation((...args) => {
      lines.push(args.map(arg => (typeof arg === 'string' ? arg : inspect(arg, { depth: 12, showHidden: true }))).join(' '));
    });
  }
  return lines;
}

function expectNoCredential(text) {
  expect(text).not.toContain(PASSWORD);
  expect(text).not.toContain(BASIC);
  expect(text).not.toContain('session-cookie-fixture');
}

describe('setCurrentApplication error containment', () => {
  test('throws only sanitized status, code and message for a real failing UI request', async () => {
    const url = await fakeServiceNow();
    const client = new ServiceNowClient(url, USERNAME, PASSWORD);
    const logs = captureConsole();

    let thrown;
    try {
      await client.setCurrentApplication(APP_ID);
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(Error);
    expect(thrown.message).toContain('ServiceNow server error (500)');
    expect(thrown.status).toBe(500);
    expect(thrown.original_error).toBeUndefined();
    expect(thrown.config).toBeUndefined();
    expect(thrown.response).toBeUndefined();
    expect(thrown.request).toBeUndefined();
    expectNoCredential(inspect(thrown, { depth: 12, showHidden: true }));
    expectNoCredential(JSON.stringify(thrown));
    expectNoCredential(logs.join('\n'));
  });

  test('redacts credential-bearing headers and config injected into a transport error', async () => {
    const url = await fakeServiceNow();
    const client = new ServiceNowClient(url, USERNAME, PASSWORD);
    const logs = captureConsole();
    jest.spyOn(axios, 'create').mockImplementation(config => ({
      get: async () => ({ data: '' }),
      put: async () => {
        const error = new Error(`connect failed with Authorization: ${config.headers.Authorization}`);
        error.code = 'ECONNRESET';
        error.config = config;
        error.request = { _header: `PUT / HTTP/1.1\r\nAuthorization: ${config.headers.Authorization}\r\n` };
        error.toJSON = () => ({ message: error.message, config });
        throw error;
      }
    }));

    let thrown;
    try {
      await client.setCurrentApplication(APP_ID);
    } catch (error) {
      thrown = error;
    }

    expect(thrown.code).toBe('ECONNRESET');
    expect(thrown.message).toContain('[redacted]');
    expect(thrown.original_error).toBeUndefined();
    expectNoCredential(inspect(thrown, { depth: 12, showHidden: true }));
    expectNoCredential(JSON.stringify(thrown));
    expectNoCredential(logs.join('\n'));
  });

  test('SN-Set-Current-Application tool output and logs never contain credentials', async () => {
    const url = await fakeServiceNow();
    const client = new ServiceNowClient(url, USERNAME, PASSWORD);
    const server = await createMcpServer(client);
    const logs = captureConsole();

    const result = await server._requestHandlers.get('tools/call')({
      method: 'tools/call',
      params: { name: 'SN-Set-Current-Application', arguments: { app_sys_id: APP_ID } }
    }, {});

    const text = result.content.map(item => item.text).join('\n');
    expect(text).toContain('Failed to set current application');
    expect(text).toContain('500');
    expectNoCredential(text);
    expectNoCredential(logs.join('\n'));
  });
});
