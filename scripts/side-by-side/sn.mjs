// Minimal REST helper pinned to dev451274 (never any other host).
// Library: makeSn(password). CLI: node sn.mjs GET|DELETE <path?query>
//   (needs HPM_SBS_JS_PREFIX = the 6.2.0 install dir, for the keychain read).
// DELETE only accepts /api/now/table/<table>/<32-hex sys_id> with no query.
import { loadPassword } from './creds.mjs';

export const HOST = 'dev451274.service-now.com';
const BASE = 'https://' + HOST;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function makeSn(password) {
  const auth = 'Basic ' + Buffer.from('admin:' + password).toString('base64');
  async function req(method, p, { tries = 4 } = {}) {
    if (!['GET', 'DELETE'].includes(method)) throw new Error('method not allowed: ' + method);
    if (!p.startsWith('/api/')) throw new Error('path must start with /api/');
    if (/javascript:/i.test(decodeURIComponent(p))) throw new Error('no javascript: in queries');
    const url = new URL(BASE + p);
    if (url.host !== HOST || url.protocol !== 'https:') throw new Error('host pin');
    if (method === 'DELETE') {
      if (!/^\/api\/now\/table\/[a-z0-9_]+\/[0-9a-f]{32}$/.test(url.pathname) || url.search)
        throw new Error('DELETE must target /api/now/table/<table>/<sys_id>, no query');
    }
    let r, text;
    for (let i = 0; i < tries; i++) {
      r = await fetch(url, { method, headers: { Authorization: auth, Accept: 'application/json' }, redirect: 'manual' });
      text = await r.text();
      if (![500, 502, 503].includes(r.status)) break; // snow_adc blips (P4 N2)
      await sleep(3000 * (i + 1));
    }
    let json = null;
    try { json = JSON.parse(text); } catch { /* HTML or empty */ }
    return { status: r.status, total: r.headers.get('x-total-count'), text, json };
  }
  return {
    get: (p) => req('GET', p),
    /** Table API rows. */
    async rows(table, query, fields, limit = 200) {
      const q = new URLSearchParams({ sysparm_query: query, sysparm_limit: String(limit) });
      if (fields) q.set('sysparm_fields', fields);
      const r = await req('GET', `/api/now/table/${table}?${q}`);
      if (r.status !== 200 || !r.json) throw new Error(`${table} query -> HTTP ${r.status}: ${r.text.slice(0, 120)}`);
      return r.json.result;
    },
    /** Aggregate count via the stats API (does not depend on row ACL paging). */
    async count(table, query) {
      const q = new URLSearchParams({ sysparm_count: 'true' });
      if (query) q.set('sysparm_query', query);
      const r = await req('GET', `/api/now/stats/${table}?${q}`);
      if (r.status !== 200 || !r.json) throw new Error(`${table} count -> HTTP ${r.status}: ${r.text.slice(0, 120)}`);
      return Number(r.json.result.stats.count);
    },
    del: (table, sysId) => req('DELETE', `/api/now/table/${table}/${sysId}`),
  };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [method, p] = process.argv.slice(2);
  const prefix = process.env.HPM_SBS_JS_PREFIX;
  if (!prefix) throw new Error('set HPM_SBS_JS_PREFIX to the 6.2.0 install dir');
  const sn = makeSn(loadPassword(prefix));
  const r = method === 'DELETE'
    ? await sn.del(...p.replace(/^\/api\/now\/table\//, '').split('/'))
    : await sn.get(p);
  console.error(`${method} ${p.split('?')[0]} -> HTTP ${r.status}`);
  process.stdout.write(r.text);
}
