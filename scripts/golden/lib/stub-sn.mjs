// Deterministic in-process stand-in for a ServiceNow instance (loopback HTTP).
// Records every request the server under test makes; answers with canned,
// reproducible payloads. No network beyond 127.0.0.1.
import http from 'node:http';
import crypto from 'node:crypto';

const FIXED_TIME = '2026-01-15 12:00:00';

export function sysId(kind, n = 1) {
  return crypto.createHash('sha1').update(`golden:${kind}:${n}`).digest('hex').slice(0, 32);
}

/** Deterministic canned row for a table. */
export function makeRow(table, n = 1, overrides = {}) {
  const base = {
    sys_id: sysId(table, n),
    sys_created_on: FIXED_TIME,
    sys_updated_on: FIXED_TIME,
    sys_created_by: 'golden.user',
    sys_updated_by: 'golden.user',
    sys_class_name: table
  };
  const pad = String(n).padStart(7, '0');
  switch (table) {
    case 'incident':
      Object.assign(base, { number: `INC${pad}`, short_description: `Golden incident ${n}`, state: '1', priority: '3', urgency: '2', impact: '2', category: 'software', caller_id: sysId('sys_user', 1), assigned_to: '', assignment_group: '' });
      break;
    case 'problem':
      Object.assign(base, { number: `PRB${pad}`, short_description: `Golden problem ${n}`, state: '1', priority: '3' });
      break;
    case 'change_request':
      Object.assign(base, { number: `CHG${pad}`, short_description: `Golden change ${n}`, state: '-5', type: 'normal', approval: 'not requested', priority: '3' });
      break;
    case 'sys_user':
      Object.assign(base, { user_name: `golden.user${n}`, name: `Golden User ${n}`, first_name: 'Golden', last_name: `User ${n}`, email: `golden.user${n}@example.invalid`, active: 'true' });
      break;
    case 'sys_user_group':
      Object.assign(base, { name: `Golden Group ${n}`, description: 'Golden group', active: 'true' });
      break;
    case 'cmdb_ci':
      Object.assign(base, { name: `golden-ci-${n}`, sys_class_name: 'cmdb_ci_server', operational_status: '1' });
      break;
    case 'sys_update_set':
      Object.assign(base, { name: `Golden Update Set ${n}`, description: 'Golden update set', state: 'in progress', application: sysId('sys_scope', 1), is_default: 'false', parent: '', base_update_set: '', sys_created_by: 'golden.user' });
      break;
    case 'sys_scope':
    case 'sys_app':
      Object.assign(base, { name: `Golden App ${n}`, scope: `x_golden_app${n}`, version: '1.0.0', active: 'true' });
      break;
    case 'sys_dictionary':
      Object.assign(base, { name: 'incident', element: `field_${n}`, column_label: `Field ${n}`, internal_type: 'string', max_length: '40', mandatory: 'false', read_only: 'false', reference: '', active: 'true', default_value: '' });
      break;
    case 'sys_db_object':
      Object.assign(base, { name: 'incident', label: 'Incident', super_class: sysId('sys_db_object', 2), sys_package: '', is_extendable: 'true' });
      break;
    case 'sys_trigger':
      Object.assign(base, { name: `golden trigger ${n}`, trigger_type: '0', state: '0' });
      break;
    case 'sc_cat_item':
      Object.assign(base, { name: `Golden Item ${n}`, short_description: 'Golden catalog item', active: 'true', category: sysId('sc_category', 1) });
      break;
    case 'sc_category':
      Object.assign(base, { title: `Golden Category ${n}`, description: 'Golden category', active: 'true' });
      break;
    default:
      Object.assign(base, { name: `golden-${table}-${n}` });
  }
  return { ...base, ...overrides };
}

function parseBody(raw, contentType = '') {
  if (!raw) return { raw: null, json: null };
  if (/json/i.test(contentType)) {
    try { return { raw, json: JSON.parse(raw) }; } catch { /* fall through */ }
  }
  return { raw, json: null };
}

// Transport-incidental headers are not part of the behavioural contract.
const DROPPED_HEADERS = new Set(['connection', 'host', 'content-length', 'accept-encoding']);
// Credential-bearing headers are never stored: only their presence/scheme is.
const CREDENTIAL_HEADERS = new Set(['authorization', 'cookie', 'proxy-authorization', 'x-usertoken', 'x-csrf-token']);

export function describeRequest(req, rawBody) {
  const url = new URL(req.url, 'http://stub.invalid');
  const headers = {};
  for (const [name, value] of Object.entries(req.headers)) {
    if (DROPPED_HEADERS.has(name) || CREDENTIAL_HEADERS.has(name)) continue;
    headers[name] = value;
  }
  const auth = req.headers.authorization;
  const credentials = {
    scheme: auth ? String(auth).split(' ')[0].toLowerCase() : 'none',
    sessionCookie: Boolean(req.headers.cookie),
    userToken: Boolean(req.headers['x-usertoken'])
  };
  const { raw, json } = parseBody(rawBody, req.headers['content-type']);
  let form = null;
  if (raw && /x-www-form-urlencoded/i.test(req.headers['content-type'] || '')) {
    form = [...new URLSearchParams(raw).entries()];
  }
  return {
    method: req.method,
    path: url.pathname,
    rawQuery: url.search.replace(/^\?/, ''),
    query: [...url.searchParams.entries()],
    headers,
    credentials,
    body: json ?? (form ? undefined : raw),
    ...(form ? { form } : {})
  };
}

/**
 * @param {object} options
 * @param {(ctx) => ([number, any, object?] | null)} [options.routes] scenario-specific handler, tried first
 */
export async function startStub({ host = '127.0.0.1' } = {}) {
  const state = {
    routes: null,      // scenario overrides: (ctx) => [status, body, headers] | null
    seq: 0,            // per-scenario counter for created sys_ids
    pickerUpdateSet: null,
    pickerApp: null,
    requests: [],
    sessions: 0
  };

  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', c => chunks.push(c));
    req.on('end', () => {
      const raw = Buffer.concat(chunks).toString('utf8');
      const described = describeRequest(req, raw);
      state.requests.push(described);
      const ctx = { req: described, state, makeRow, sysId };
      let answer = null;
      try {
        if (state.routes) answer = state.routes(ctx);
        if (!answer) answer = defaultRoutes(ctx);
      } catch (error) {
        answer = [500, { error: { message: `stub failure: ${error.message}`, detail: 'golden stub' } }];
      }
      const [status, body, extra = {}] = answer;
      const isText = typeof body === 'string';
      const headers = { 'content-type': isText ? 'text/html;charset=UTF-8' : 'application/json;charset=UTF-8', ...extra };
      res.writeHead(status, headers);
      res.end(status === 204 || body === undefined ? undefined : (isText ? body : JSON.stringify(body)));
    });
  });
  await new Promise(resolve => server.listen(0, host, resolve));
  const port = server.address().port;
  return {
    host,
    port,
    origin: `http://${host}:${port}`,
    state,
    reset(routes = null) {
      state.routes = routes;
      state.seq = 0;
      state.sessions = 0;
      state.pickerUpdateSet = null;
      state.pickerApp = null;
      state.requests = [];
    },
    take() { const out = state.requests; state.requests = []; return out; },
    close: () => new Promise(resolve => { server.closeAllConnections?.(); server.close(resolve); })
  };
}

function nextCreated(ctx, table, body) {
  ctx.state.seq += 1;
  return { ...makeRow(table, 100 + ctx.state.seq), ...(body || {}), sys_id: sysId(`created:${table}`, ctx.state.seq) };
}

const TABLE_RE = /^\/api\/now\/table\/([^/]+)(?:\/([^/]+))?$/;

/** Default behaviour: realistic Table API, update-set pickers, session bootstrap. */
export function defaultRoutes(ctx) {
  const { req, state } = ctx;
  const { method, path } = req;

  if (method === 'GET' && path === '/') {
    state.sessions += 1;
    return [200, '<html><body>golden</body></html>', { 'set-cookie': [`JSESSIONID=golden-session-${state.sessions}; Path=/; HttpOnly`, 'glide_user_route=golden-route; Path=/'] }];
  }

  const m = TABLE_RE.exec(path);
  if (m) {
    const [, table, id] = m;
    if (method === 'GET' && !id) {
      const limit = Number(req.query.find(([k]) => k === 'sysparm_limit')?.[1] ?? 3) || 3;
      const query = req.query.find(([k]) => k === 'sysparm_query')?.[1] ?? '';
      const rows = [];
      const num = /number=([A-Z]+\d+)/.exec(query);
      const sid = /sys_id=([0-9a-f]{32})/.exec(query);
      const count = (num || sid) ? 1 : Math.min(limit, 3);
      for (let i = 1; i <= count; i += 1) {
        const overrides = {};
        if (num) overrides.number = num[1];
        if (sid) overrides.sys_id = sid[1];
        rows.push(makeRow(table, i, overrides));
      }
      return [200, { result: rows }, { 'x-total-count': String(rows.length) }];
    }
    if (method === 'GET' && id) return [200, { result: makeRow(table, 1, { sys_id: id }) }];
    if (method === 'POST') return [201, { result: nextCreated(ctx, table, req.body) }];
    if (method === 'PATCH' || method === 'PUT') return [200, { result: { ...makeRow(table, 1, { sys_id: id }), ...(req.body || {}) } }];
    if (method === 'DELETE') return [204, undefined];
  }

  if (path === '/api/now/ui/concoursepicker/updateset') {
    state.pickerUpdateSet ??= { sysId: sysId('picker', 1), name: 'Golden current update set' };
    if (method === 'PUT') {
      state.pickerUpdateSet = { sysId: req.body?.sysId, name: req.body?.name };
      return [200, { result: { ok: true } }];
    }
    return [200, { result: { current: state.pickerUpdateSet } }];
  }
  if (path === '/api/now/ui/concoursepicker/application') {
    state.pickerApp ??= 'global';
    if (method === 'PUT') {
      state.pickerApp = req.body?.app_id;
      return [200, { result: { ok: true } }];
    }
    return [200, { result: { current: state.pickerApp, list: [{ sysId: 'global', name: 'Global' }, { sysId: sysId('sys_app', 1), name: 'Golden App 1' }] } }];
  }

  return [404, { error: { message: `No Processor or Table API for ${method} ${path}`, detail: 'golden stub: unrouted' }, status: 'failure' }];
}

/** Loopback stand-in for raw.githubusercontent.com (docs tools). Static route table. */
export async function startRawStub(routes) {
  const requests = [];
  const server = http.createServer((req, res) => {
    requests.push(describeRequest(req, ''));
    const body = routes[req.url];
    if (body === undefined) {
      res.writeHead(404, { 'content-type': 'text/plain' });
      res.end('not found');
      return;
    }
    res.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' });
    res.end(body);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  return {
    port,
    base: `http://127.0.0.1:${port}`,
    take() { const out = requests.slice(); requests.length = 0; return out; },
    close: () => new Promise(resolve => { server.closeAllConnections?.(); server.close(resolve); })
  };
}
