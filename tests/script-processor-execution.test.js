// SN-Execute-Background-Script through the /sys.scripts.do processor, against
// a local fake ServiceNow. The fake models the processor's real contract:
// CSRF tokens belong to a cookie session, the processor answers 200 for
// everything (auth failures included), and the result is HTML.
import http from 'node:http';
import { afterEach, beforeEach, describe, expect, jest, test } from '@jest/globals';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { ServiceNowClient } from '../src/servicenow-client.js';
import { createMcpServer } from '../src/mcp-server-consolidated.js';
import { parseScriptResponse } from '../src/script-processor.js';

const HISTORY_ID = '1'.repeat(32);
const ROLLBACK_ID = '2'.repeat(32);
const APP_ID = 'a'.repeat(32);
const TRIGGER_ID = 't'.repeat(32);
const SESSION_ID = 'SESSION123';
const CSRF_TOKEN = 'csrf-token-for-session';

function processorPage({ scope = 'global', lines = [], historyId = HISTORY_ID } = {}) {
  const pre = lines.map((line) => `${line}<BR/>`).join('');
  const link = historyId
    ? `<A HREF="sys_script_execution_history.do?sys_id=${historyId}&amp;sysparm_view=">Script execution history and recovery available here</A><BR/>`
    : '';
  return `<HTML><BODY><HR/><PRE>${pre}</PRE><HR/>[0:00:00.004] Script completed in scope ${scope}: script<BR/>${link}</BODY></HTML>`;
}

const LOGIN_PAGE = '<html><head><title>Log In | ServiceNow</title></head><body><form action="login.do" method="post"><input name="user_name"><input name="user_password" type="password"></form></body></html>';

async function fakeServiceNow({ processor, userToken = null } = {}) {
  const requests = [];
  let triggerEndMarker = null;
  const server = http.createServer((req, res) => {
    let raw = '';
    req.setEncoding('utf8');
    req.on('data', (chunk) => { raw += chunk; });
    req.on('end', async () => {
      const url = new URL(req.url, 'http://fake');
      const record = {
        method: req.method,
        path: url.pathname,
        query: url.searchParams,
        cookie: req.headers.cookie || '',
        authorization: req.headers.authorization || '',
        userTokenHeader: req.headers['x-usertoken'] || null,
        body: raw
      };
      requests.push(record);
      const json = (status, payload, headers = {}) => {
        res.writeHead(status, { 'content-type': 'application/json', ...headers });
        res.end(JSON.stringify(payload));
      };
      const sessionCookie = `glide_session_store=${SESSION_ID}; Path=/; HttpOnly`;

      if (req.method === 'POST' && url.pathname === '/angular.do') {
        return json(userToken ? 401 : 200, {}, {
          'set-cookie': sessionCookie,
          ...(userToken ? { 'x-usertoken-response': userToken } : {})
        });
      }
      if (req.method === 'GET' && url.pathname === '/api/now/ui/user/current_user') {
        return json(200, { result: { user_name: 'admin' } }, { 'set-cookie': sessionCookie });
      }
      if (req.method === 'GET' && url.pathname === '/api/now/table/sys_user_session') {
        // A request outside the cookie session cannot see this session's token.
        const inSession = record.cookie.includes(`glide_session_store=${SESSION_ID}`);
        const matches = url.searchParams.get('sysparm_query') === `id=${SESSION_ID}`;
        return json(200, { result: inSession && matches ? [{ csrf_token: CSRF_TOKEN, id: SESSION_ID }] : [] });
      }
      if (req.method === 'POST' && url.pathname === '/sys.scripts.do') {
        const reply = await processor(record);
        if (reply.hang) return undefined;
        res.writeHead(reply.status ?? 200, { 'content-type': 'text/html', ...(reply.headers || {}) });
        return res.end(reply.body ?? '');
      }
      if (req.method === 'GET' && url.pathname === '/api/now/table/sys_scope') {
        const query = url.searchParams.get('sysparm_query');
        const known = query === 'scope=x_acme_app' || query === `sys_id=${APP_ID}`;
        return json(200, { result: known ? [{ sys_id: APP_ID, scope: 'x_acme_app', name: 'Acme', active: 'true' }] : [] });
      }
      if (req.method === 'GET' && url.pathname === '/api/now/table/sys_script_execution_history') {
        return json(200, { result: [{ rollback_context: { value: ROLLBACK_ID, link: 'https://fake/x' } }] });
      }
      if (url.pathname.startsWith('/api/now/table/sys_trigger')) {
        if (req.method === 'POST') return json(201, { result: { sys_id: TRIGGER_ID, name: 'MCP_Script_1' } });
        const script = JSON.parse(raw || '{}').script || '';
        triggerEndMarker = /gs\.info\('([^']+:END)'/.exec(script)?.[1] ?? triggerEndMarker;
        return json(200, { result: { sys_id: TRIGGER_ID } });
      }
      if (req.method === 'GET' && url.pathname === '/api/now/table/syslog') {
        const end = { message: `${triggerEndMarker}${JSON.stringify({ ok: true })}`, sys_created_on: '2099-01-01 00:00:00' };
        return json(200, { result: triggerEndMarker ? [end] : [] });
      }
      return json(404, { error: { message: `unhandled ${req.method} ${url.pathname}` } });
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const url = `http://127.0.0.1:${server.address().port}`;
  return {
    url,
    requests,
    scriptPosts: () => requests.filter((r) => r.method === 'POST' && r.path === '/sys.scripts.do'),
    triggerPosts: () => requests.filter((r) => r.method === 'POST' && r.path === '/api/now/table/sys_trigger'),
    close: () => new Promise((resolve) => { server.closeAllConnections?.(); server.close(resolve); })
  };
}

const open = [];
async function fake(options) {
  const f = await fakeServiceNow(options);
  open.push(f);
  return f;
}

async function callTool(serviceNowClient, args) {
  const server = await createMcpServer(serviceNowClient);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);
  const client = new Client({ name: 'script-processor-test', version: '1.0.0' });
  await client.connect(clientTransport);
  try {
    return await client.callTool({ name: 'SN-Execute-Background-Script', arguments: args });
  } finally {
    await client.close();
    await server.close();
  }
}

const textOf = (result) => result.content.map((c) => c.text).join('\n');
const formOf = (request) => new URLSearchParams(request.body);
const basicClient = (f) => new ServiceNowClient(f.url, 'admin', 'inert-test-password');

beforeEach(() => {
  jest.spyOn(console, 'error').mockImplementation(() => {});
});
afterEach(async () => {
  jest.restoreAllMocks();
  for (const f of open.splice(0)) await f.close();
});

describe('parseScriptResponse', () => {
  test('global run: prefixed output, execution errors, decoded entities and the history id', () => {
    const result = parseScriptResponse(processorPage({
      lines: [
        '*** Script: hello &lt;world&gt; &amp; more',
        'Background message, type:info, message: session noise',
        'Script execution error: Script Identifier: null.null.script, Error Description: ReferenceError: "nope" is not defined., Script ES Level: 0'
      ]
    }));
    expect(result).toMatchObject({
      ok: false,
      scope: 'global',
      output: ['hello <world> & more'],
      errors: ['ReferenceError: "nope" is not defined.'],
      log: ['Background message, type:info, message: session noise'],
      executionHistoryId: HISTORY_ID
    });
  });

  test('scoped run: scope-name output prefix and a scoped syntax error', () => {
    const result = parseScriptResponse(processorPage({
      scope: 'x_acme_app',
      historyId: null,
      lines: ['x_acme_app: scoped hello', 'SyntaxError: missing ; before statement\n  at line 1']
    }));
    expect(result.ok).toBe(false);
    expect(result.scope).toBe('x_acme_app');
    expect(result.output).toEqual(['scoped hello']);
    expect(result.errors).toEqual(['SyntaxError: missing ; before statement']);
    expect(result.executionHistoryId).toBeUndefined();
  });

  test.each([['login page', LOGIN_PAGE], ['empty body', ''], ['form without a result', '<html><pre>x</pre></html>']])(
    'is not a script result: %s', (_label, body) => {
      expect(parseScriptResponse(body)).toBeNull();
    }
  );

  test('real scoped-run response captured from a PDI', () => {
    const body = "[0:00:00.590] <HTML><BODY>Script completed in scope x_879483_hap610: script<HR/>Script execution history <A target='blank' HREF='sys_script_execution_history.do?sys_id=732ae563c3c4d35014b076d4e40131c8'>available here</A><HR/><PRE>Script execution error: Script Identifier: null.null.script, Error Description: Function print is not allowed in scope x_879483_hap610. Use gs.debug() or gs.info() instead, Script ES Level: 250<BR/>Evaluator.evaluateString() problem: com.glide.script.fencing.MethodNotAllowedException: Function print is not allowed in scope x_879483_hap610<BR/></PRE></BODY></HTML>";

    const result = parseScriptResponse(body);

    expect(result).toMatchObject({
      ok: false,
      scope: 'x_879483_hap610',
      output: [],
      executionHistoryId: '732ae563c3c4d35014b076d4e40131c8'
    });
    expect(result.errors).toHaveLength(1);
    expect(result.errors[0]).toMatch(/Function print is not allowed/);
  });

  test('nested, unterminated and comment markup never survives; encoded text does', () => {
    const result = parseScriptResponse(processorPage({
      lines: [
        '*** Script: a<scr<b>ipt>b',
        '*** Script: c<!--<!---->-->d',
        '*** Script: e<!-- x > y -->f',
        '*** Script: &lt;script&gt;kept&lt;/script&gt;',
        '*** Script: tail<script'
      ]
    }));
    expect(result.output).toEqual(['aipt>b', 'c-->d', 'ef', '<script>kept</script>', 'tail']);
  });
});

describe('executeScriptViaProcessor request', () => {
  test('posts once in the CSRF session with rollback, scope and a one-line reason', async () => {
    const f = await fake({ processor: () => ({ body: processorPage({ scope: 'x_acme_app', lines: ['x_acme_app: done'] }) }) });

    const result = await basicClient(f).executeScriptViaProcessor("gs.info('done');", {
      scope: 'x_acme_app',
      reason: 'backfill\n priority  values'
    });

    const [post] = f.scriptPosts();
    expect(f.scriptPosts()).toHaveLength(1);
    const form = formOf(post);
    expect(form.get('script')).toBe("gs.info('done');\n// happy-platform-mcp: backfill priority values");
    expect(form.get('sysparm_ck')).toBe(CSRF_TOKEN);
    expect(post.cookie).toContain(`glide_session_store=${SESSION_ID}`);
    expect(form.get('runscript')).toBe('Run script');
    expect(form.get('quota_managed_transaction')).toBe('on');
    expect(form.get('record_for_rollback')).toBe('on');
    expect(form.get('sys_scope')).toBe(APP_ID);
    expect(result).toMatchObject({
      ok: true,
      method: 'processor',
      output: ['done'],
      errors: [],
      scope: 'x_acme_app',
      scopeId: APP_ID,
      executionHistoryId: HISTORY_ID,
      rollbackContextId: ROLLBACK_ID,
      links: {
        executionHistory: `/sys_script_execution_history.do?sys_id=${HISTORY_ID}`,
        rollbackContext: `/sys_rollback_context.do?sys_id=${ROLLBACK_ID}`
      }
    });
  });

  test('omits rollback recording and scope when not requested', async () => {
    const f = await fake({ processor: () => ({ body: processorPage({ lines: ['*** Script: ok'] }) }) });

    await basicClient(f).executeScriptViaProcessor('gs.print("ok");', { recordForRollback: false });

    const form = formOf(f.scriptPosts()[0]);
    expect(form.has('record_for_rollback')).toBe(false);
    expect(form.has('sys_scope')).toBe(false);
  });

  test('uses the basic-auth user token as sysparm_ck when the instance exposes one', async () => {
    const f = await fake({ userToken: 'user-token-1', processor: () => ({ body: processorPage() }) });

    await basicClient(f).executeScriptViaProcessor('gs.print(1);');

    const [post] = f.scriptPosts();
    expect(formOf(post).get('sysparm_ck')).toBe('user-token-1');
    expect(post.userTokenHeader).toBe('user-token-1');
  });

  test('rejects an unknown scope before posting the script', async () => {
    const f = await fake({ processor: () => ({ body: processorPage() }) });

    await expect(basicClient(f).executeScriptViaProcessor('gs.print(1);', { scope: 'x_missing' }))
      .rejects.toThrow(/No application found with scope 'x_missing'/);
    await expect(basicClient(f).executeScriptViaProcessor('gs.print(1);', { scope: 'bad^scope' }))
      .rejects.toThrow(/scope must be/);
    expect(f.scriptPosts()).toHaveLength(0);
  });

  test('an OAuth 401 on the script POST is not replayed', async () => {
    const f = await fake({ processor: () => ({ status: 401, body: '' }) });
    const postToken = jest.fn(async () => ({ access_token: 'refreshed-token', expires_in: 1800 }));
    const client = new ServiceNowClient(f.url, null, null, {
      authType: 'oauth', grantType: 'client_credentials', clientId: 'cid', clientSecret: 'inert-secret', postToken
    });
    client.oauthToken = 'cached-token';
    client.oauthTokenExpiry = Date.now() + 300000;

    await expect(client.executeScriptViaProcessor('gs.print(1);'))
      .rejects.toMatchObject({ code: 'SCRIPT_PROCESSOR_NOT_RUN' });
    expect(f.scriptPosts()).toHaveLength(1);
    expect(f.scriptPosts()[0].authorization).toBe('Bearer cached-token');
    expect(formOf(f.scriptPosts()[0]).get('sysparm_ck')).toBe(CSRF_TOKEN);
    expect(postToken).not.toHaveBeenCalled();
  });

  test('a timeout is an unknown outcome and the script is posted exactly once', async () => {
    const f = await fake({ processor: () => ({ hang: true }) });

    await expect(basicClient(f).executeScriptViaProcessor('gs.sleep(10000);', { timeoutMs: 200 }))
      .rejects.toMatchObject({ code: 'SCRIPT_PROCESSOR_UNKNOWN_OUTCOME' });
    expect(f.scriptPosts()).toHaveLength(1);
    expect(f.triggerPosts()).toHaveLength(0);
  });

  test('a redirect to login means the script did not run', async () => {
    const f = await fake({ processor: () => ({ status: 302, headers: { location: '/login.do' } }) });

    await expect(basicClient(f).executeScriptViaProcessor('gs.print(1);'))
      .rejects.toMatchObject({ code: 'SCRIPT_PROCESSOR_NOT_RUN' });
    expect(f.scriptPosts()).toHaveLength(1);
  });
});

describe('SN-Execute-Background-Script', () => {
  test('reports processor output, scope, history and rollback context without using sys_trigger', async () => {
    const f = await fake({ processor: () => ({ body: processorPage({ lines: ['*** Script: 42 incidents'] }) }) });

    const result = await callTool(basicClient(f), { script: 'gs.print(42);', description: 'count' });

    expect(result.isError).toBe(false);
    const text = textOf(result);
    expect(text).toContain('42 incidents');
    expect(text).toContain('Scope: global');
    expect(text).toContain(`/sys_script_execution_history.do?sys_id=${HISTORY_ID}`);
    expect(text).toContain(`/sys_rollback_context.do?sys_id=${ROLLBACK_ID}`);
    expect(text).toMatch(/rolled back/);
    expect(f.triggerPosts()).toHaveLength(0);
  });

  test('script errors are reported as an error result', async () => {
    const f = await fake({ processor: () => ({ body: processorPage({ lines: ['Script compilation error: Unexpected token'] }) }) });

    const result = await callTool(basicClient(f), { script: 'gs.print(;' });

    expect(result.isError).toBe(true);
    expect(textOf(result)).toContain('Unexpected token');
    expect(f.triggerPosts()).toHaveLength(0);
  });

  test('HTTP 403 falls back to sys_trigger exactly once and says why', async () => {
    const f = await fake({ processor: () => ({ status: 403, body: 'forbidden' }) });

    const result = await callTool(basicClient(f), { script: "gs.info('x');" });

    expect(f.scriptPosts()).toHaveLength(1);
    expect(f.triggerPosts()).toHaveLength(1);
    expect(result.isError).toBe(false);
    const text = textOf(result);
    expect(text).toMatch(/sys_trigger fallback/);
    expect(text).toContain('HTTP 403');
  });

  test('a 200 login page is treated as not run and falls back', async () => {
    const f = await fake({ processor: () => ({ body: LOGIN_PAGE }) });

    const result = await callTool(basicClient(f), { script: "gs.info('x');" });

    expect(f.scriptPosts()).toHaveLength(1);
    expect(f.triggerPosts()).toHaveLength(1);
    expect(textOf(result)).toMatch(/sys_trigger fallback/);
  });

  test.each([500, 502])('HTTP %i is an unknown outcome: no fallback, posted once', async (status) => {
    const f = await fake({ processor: () => ({ status, body: 'error' }) });

    const result = await callTool(basicClient(f), { script: "gs.info('x');" });

    expect(result.isError).toBe(true);
    expect(textOf(result)).toMatch(/Unknown outcome/);
    expect(textOf(result)).toMatch(/sys_script_execution_history/);
    expect(f.scriptPosts()).toHaveLength(1);
    expect(f.triggerPosts()).toHaveLength(0);
  });

  test.each(['not authorized', '  NOT AUTHORIZED\n'])(
    'a 200 %p body in a non-global scope is NOT run and never falls back', async (body) => {
      const f = await fake({ processor: () => ({ body }) });

      const result = await callTool(basicClient(f), { script: "gs.info('x');", scope: 'x_acme_app' });

      expect(result.isError).toBe(true);
      expect(textOf(result)).toMatch(/NOT run/);
      expect(textOf(result)).toMatch(/not authorized/);
      expect(f.scriptPosts()).toHaveLength(1);
      expect(f.triggerPosts()).toHaveLength(0);
    }
  );

  test('a 200 "not authorized" body in global scope falls back to sys_trigger', async () => {
    const f = await fake({ processor: () => ({ body: 'not authorized' }) });

    const result = await callTool(basicClient(f), { script: "gs.info('x');" });

    expect(f.scriptPosts()).toHaveLength(1);
    expect(f.triggerPosts()).toHaveLength(1);
    expect(textOf(result)).toMatch(/sys_trigger fallback/);
  });

  test('a non-global scope is never handed to the sys_trigger fallback', async () => {
    const f = await fake({ processor: () => ({ status: 403, body: '' }) });

    const result = await callTool(basicClient(f), { script: "gs.info('x');", scope: 'x_acme_app' });

    expect(result.isError).toBe(true);
    expect(textOf(result)).toMatch(/NOT run/);
    expect(f.triggerPosts()).toHaveLength(0);
  });

  test('wait: false schedules via sys_trigger without touching the processor', async () => {
    const f = await fake({ processor: () => ({ body: processorPage() }) });

    const result = await callTool(basicClient(f), { script: "gs.info('x');", wait: false });

    expect(result.isError).toBe(false);
    expect(textOf(result)).toMatch(/scheduled/);
    expect(f.scriptPosts()).toHaveLength(0);
    expect(f.triggerPosts()).toHaveLength(1);
  });
});
