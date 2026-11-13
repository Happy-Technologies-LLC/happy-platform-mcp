// Scrubbing (secrets, hosts, paths) and normalization (listed nondeterministic
// fields) for golden output. Every rule has an id and a doc line; hit counts are
// written to NORMALIZATION.json so reviewers can see exactly what was touched.
//
// Two families:
//   scrub rules     - remove credentials, cookies, tokens, real hostnames, local paths.
//   normalize rules - replace values that legitimately differ run to run
//                     (timestamps, durations, generated ids, markers, request ids).
// Nothing else is rewritten. Matching is applied to every JSON string value
// (and object key) of a captured document, never to the raw serialization.

export const SCRUB_RULES = [
  { id: 'S-SN-HOST', doc: 'Any hostname under the ServiceNow cloud domains (patterns in lib/scrub.mjs)', re: /\b[a-z0-9][a-z0-9-]*\.(?:service-now|servicenowservices)\.com\b/gi, to: '{{SN_HOST}}' },
  { id: 'S-REAL-INSTANCE', doc: 'Names of real (non-fixture) ServiceNow instances that must never appear in goldens; the exact list lives in lib/scrub.mjs', re: /\b(?:dev451274|ccf[a-z0-9]*)\b/gi, to: '{{REAL_INSTANCE}}' },
  { id: 'S-AUTH-BASIC', doc: 'HTTP basic-scheme credential value', re: /\bBasic\s+[A-Za-z0-9+/=]{8,}/g, to: 'Basic {{CREDENTIAL}}' },
  { id: 'S-AUTH-BEARER', doc: 'HTTP bearer-scheme token value', re: /\bBearer\s+[A-Za-z0-9._~+/=-]{8,}/g, to: 'Bearer {{CREDENTIAL}}' },
  { id: 'S-PASSWORD-PARAM', doc: 'Query/form parameters that carry passwords, client secrets or tokens (value replaced)', re: /\b(password|client_secret|refresh_token|access_token)=[^&\s"']+/gi, to: '$1={{CREDENTIAL}}' },
  { id: 'S-COOKIE-VALUE', doc: 'Cookie name=value pairs for session cookies', re: /\b(JSESSIONID|glide_user_route|glide_session_store|glide_user_activity|BIGipServer\w*)=[^;\s"']+/g, to: '$1={{COOKIE}}' }
];

export const NORMALIZE_RULES = [
  { id: 'N-ISO-TIMESTAMP', doc: 'ISO-8601 timestamps from new Date().toISOString()', re: /\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\b/g, to: '{{ISO_TIMESTAMP}}' },
  { id: 'N-FILE-TIMESTAMP', doc: 'Filename-safe timestamps (fix scripts: 2026-01-01T00-00-00-000Z)', re: /(?<![0-9])\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z(?![0-9])/g, to: '{{FILE_TIMESTAMP}}' },
  { id: 'N-SN-DATETIME', doc: 'Server-generated ServiceNow datetimes (YYYY-MM-DD HH:MM:SS) NOT equal to the stub fixed time', re: /\b(?!2026-01-15 12:00:00\b)\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\b/g, to: '{{SN_DATETIME}}' },
  { id: 'N-DURATION-MS', doc: 'Elapsed-time figures in ms', re: /\b(\d+)\s?ms\b/g, to: '{{N}}ms' },
  { id: 'N-EXEC-TIME-FIELD', doc: 'Elapsed-time fields embedded in JSON text (execution_time_ms: N)', re: /("?execution_time_ms"?\s*:\s*)\d+/g, to: '$1{{N}}' },
  { id: 'N-EPOCH-MS', doc: 'Epoch-millisecond ids embedded in generated names (MCP_Script_<ms>)', re: /\b(MCP_Script_|MCP_Workflow_|mcp_marker_)\d{10,}\b/g, to: '$1{{EPOCH_MS}}' },
  { id: 'N-RANDOM-MARKER', doc: 'Random per-execution script markers', re: /\bMCP_SCRIPT_[A-Z0-9]+_[0-9a-f]{8,}\b/g, to: 'MCP_SCRIPT_{{MARKER}}' },
  { id: 'N-TRIGGER-MARKER', doc: 'sys_trigger fallback correlation marker MCP_<epoch-ms>_<8 hex>', re: /MCP_\d{10,}_[0-9a-f]{6,}(?![0-9a-f])/g, to: 'MCP_{{EPOCH_MS}}_{{MARKER}}' },
  { id: 'N-SN-DATETIME-ENCODED', doc: 'URL-encoded datetimes in raw query strings (YYYY-MM-DD+HH%3AMM%3ASS) other than the stub fixed time', re: /(?<!\d)(?!2026-01-15\+12%3A00%3A00)\d{4}-\d{2}-\d{2}\+\d{2}%3A\d{2}%3A\d{2}\b/g, to: '{{SN_DATETIME_ENCODED}}' },
  { id: 'N-UUID', doc: 'Random UUIDs (request/session/health ids)', re: /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/g, to: '{{UUID}}' }
];

export class Scrubber {
  /**
   * @param {Array<[string,string]>} literals exact strings (longest first) -> placeholder, e.g. temp dirs, stub origins
   */
  constructor(literals = [], { rules = 'all' } = {}) {
    this.literals = [...literals]
      .filter(([value]) => typeof value === 'string' && value.length > 0)
      .sort((a, b) => b[0].length - a[0].length);
    // 'all' = scrub + normalize (behaviour captures); 'scrub' = secrets only (MCP surface,
    // which must stay byte-exact); 'none' = literal placeholders only.
    this.rules = rules === 'all' ? [...SCRUB_RULES, ...NORMALIZE_RULES] : rules === 'scrub' ? [...SCRUB_RULES] : [];
    this.hits = new Map();
  }

  _count(id, n = 1) { this.hits.set(id, (this.hits.get(id) || 0) + n); }

  string(value) {
    let out = value;
    for (const [literal, placeholder] of this.literals) {
      if (out.includes(literal)) {
        const parts = out.split(literal);
        this._count(`L:${placeholder}`, parts.length - 1);
        out = parts.join(placeholder);
      }
    }
    for (const rule of this.rules) {
      rule.re.lastIndex = 0;
      out = out.replace(rule.re, (...m) => {
        this._count(rule.id);
        return rule.to.replace(/\$(\d)/g, (_, i) => m[Number(i)] ?? '');
      });
    }
    return out;
  }

  deep(value) {
    if (typeof value === 'string') return this.string(value);
    if (Array.isArray(value)) return value.map(v => this.deep(v));
    if (value && typeof value === 'object') {
      const out = {};
      for (const [k, v] of Object.entries(value)) out[this.string(k)] = this.deep(v);
      return out;
    }
    return value;
  }

  report() {
    return Object.fromEntries([...this.hits.entries()].sort(([a], [b]) => (a < b ? -1 : 1)));
  }
}

/** Deterministic JSON: object keys sorted recursively, 2-space indent, trailing newline. */
export function stableStringify(value) {
  const sort = v => {
    if (Array.isArray(v)) return v.map(sort);
    if (v && typeof v === 'object') return Object.fromEntries(Object.keys(v).sort().map(k => [k, sort(v[k])]));
    return v;
  };
  return `${JSON.stringify(sort(value), null, 2)}\n`;
}
