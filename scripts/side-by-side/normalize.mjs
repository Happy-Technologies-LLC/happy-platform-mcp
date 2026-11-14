// Normalisation and structural diff for the side-by-side harness.
// Volatile values (timestamps of this run, ids/numbers of records each server
// created, per-run prefixes, durations, request ids) are replaced by stable
// placeholders; everything else is compared exactly.

const TS = /\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?Z?/g;
// Keys whose values vary per call by nature (never record data).
// "execution_time_ms": 123 inside pretty-printed text bodies.
const VOLATILE_TEXT = /("?(?:execution_time_ms|duration_ms|elapsed_ms|execution_time)"?\s*:\s*)[\d.]+/g;
const VOLATILE_KEY = /^(duration|duration_ms|elapsed|elapsed_ms|execution_time|response_time|request_id|requestId|call_id|callId|trace_id|traceId|timestamp)$/;

export class Normalizer {
  constructor(runId) {
    this.runId = runId;
    // side -> Map(value -> placeholder) for ids / numbers each side created.
    this.reg = { js: new Map(), rs: new Map() };
  }
  register(side, value, placeholder) {
    if (value) this.reg[side].set(String(value), placeholder);
  }
  str(side, s, { ts }) {
    let out = s.replace(new RegExp(`hpm-p5-sbs-${this.runId}-(js|rs)`, 'g'), 'hpm-p5-sbs-<RUN>-<SIDE>');
    for (const [v, ph] of this.reg[side]) out = out.split(v).join(ph);
    out = out.replace(VOLATILE_TEXT, '$1<VOL>');
    if (ts) out = out.replace(TS, '<TS>');
    return out;
  }
  value(side, v, opts, key) {
    if (typeof v === 'string') {
      if (key && VOLATILE_KEY.test(key)) return '<VOL>';
      const t = v.trim();
      if ((t.startsWith('{') && t.endsWith('}')) || (t.startsWith('[') && t.endsWith(']'))) {
        try { return { '<json-text>': this.value(side, JSON.parse(t), opts) }; } catch { /* plain text */ }
      }
      return this.str(side, v, opts);
    }
    if (Array.isArray(v)) return v.map((x) => this.value(side, x, opts));
    if (v && typeof v === 'object') {
      const o = {};
      for (const k of Object.keys(v).sort()) o[this.str(side, k, opts)] = VOLATILE_KEY.test(k) ? '<VOL>' : this.value(side, v[k], opts, k);
      return o;
    }
    return v;
  }
  /** One JSON-RPC response (or {transportError}) -> comparable value. */
  response(side, resp, opts = {}) {
    return this.value(side, resp, opts);
  }
}

/** Flatten to path -> primitive; long multi-line strings go line by line. */
export function flatten(v, p = '', out = {}) {
  if (Array.isArray(v)) {
    if (v.length === 0) out[p || '$'] = '[]';
    v.forEach((x, i) => flatten(x, `${p}[${i}]`, out));
  } else if (v && typeof v === 'object') {
    const ks = Object.keys(v);
    if (ks.length === 0) out[p || '$'] = '{}';
    for (const k of ks) flatten(v[k], p ? `${p}.${k}` : k, out);
  } else if (typeof v === 'string' && v.includes('\n')) {
    v.split('\n').forEach((line, i) => { out[`${p}#L${i + 1}`] = line; });
  } else {
    out[p || '$'] = v;
  }
  return out;
}

export function diff(a, b) {
  const fa = flatten(a), fb = flatten(b);
  const keys = [...new Set([...Object.keys(fa), ...Object.keys(fb)])];
  const out = [];
  for (const k of keys) {
    const x = fa[k], y = fb[k];
    if (x !== y) out.push({ path: k, js: k in fa ? x : '<absent>', rs: k in fb ? y : '<absent>' });
  }
  return out;
}
