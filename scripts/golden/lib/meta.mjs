// MANIFEST.json (hashes + tool coverage) and NORMALIZATION.json (every rewrite rule + hit counts).
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { SCRUB_RULES, NORMALIZE_RULES } from './scrub.mjs';
import { REPO } from './paths.mjs';

function sha(text) { return crypto.createHash('sha256').update(text).digest('hex'); }

export function buildMeta({ files, hits }, opts) {
  const pkg = JSON.parse(fs.readFileSync(path.join(REPO, 'package.json'), 'utf8'));
  const listDoc = files.get('surface/full-default.json');
  let served = [];
  if (listDoc) {
    const doc = JSON.parse(listDoc);
    const ex = doc.exchanges.find(e => e.request.method === 'tools/list');
    served = (ex?.response?.result?.tools || []).map(t => t.name).sort();
  }
  const covered = new Map();
  for (const [rel, text] of files) {
    if (!rel.startsWith('tools/')) continue;
    const doc = JSON.parse(text);
    const calls = doc.steps.filter(s => s.request.method === 'tools/call' && s.request.params?.name === doc.tool && s.response);
    if (!calls.length) continue;
    const entry = covered.get(doc.tool) || { captures: 0, withOutboundHttp: 0, files: [] };
    entry.captures += calls.length;
    entry.withOutboundHttp += calls.filter(s => s.outbound.length > 0).length;
    entry.files.push(rel);
    covered.set(doc.tool, entry);
  }
  const toolCoverage = Object.fromEntries(served.map(t => [t, covered.get(t) ? { captures: covered.get(t).captures, withOutboundHttp: covered.get(t).withOutboundHttp, scenarios: covered.get(t).files.length } : { captures: 0, withOutboundHttp: 0, scenarios: 0 }]));
  const summary = {
    toolsServed: served.length,
    toolsWithAtLeastOneCapture: Object.values(toolCoverage).filter(c => c.captures > 0).length,
    toolsWithAtLeastOneOutboundRequest: Object.values(toolCoverage).filter(c => c.withOutboundHttp > 0).length,
    toolsWithoutCapture: Object.entries(toolCoverage).filter(([, c]) => c.captures === 0).map(([t]) => t),
    unknownToolsCaptured: [...covered.keys()].filter(t => !served.includes(t)).sort()
  };
  const manifest = {
    description: 'Golden captures of the Node baseline. Immutable once committed; regenerate only via scripts/golden/capture.mjs --force with owner approval.',
    baseline: { name: pkg.name, version: pkg.version },
    stages: opts.only,
    coverage: summary,
    toolCoverage,
    files: Object.fromEntries([...files.entries()].sort(([a], [b]) => (a < b ? -1 : 1)).map(([rel, text]) => [rel, { sha256: sha(text), bytes: Buffer.byteLength(text) }]))
  };
  const normalization = {
    description: 'Everything the capture rewrites. Secrets/hosts/paths are scrubbed; only listed nondeterministic values are normalized. Matching runs on JSON string values and object keys, never on the serialized document.',
    stageRuleSets: { surface: 'scrub-only (tools/list and all protocol payloads stay byte-exact)', cli: 'scrub + normalize', tools: 'scrub + normalize' },
    literals: [
      ['loopback ServiceNow stub origin http://127.0.0.1:<port>', '{{INSTANCE_URL}} / {{INSTANCE_URL_ALT}} / {{INSTANCE_HOST}} / {{INSTANCE_HOST_ALT}}'],
      ['loopback raw-GitHub docs stub', '{{RAW_GITHUB_URL}} / {{RAW_GITHUB_HOST}}'],
      ['the fixed fake test secret and its encoded basic-auth form', '{{FAKE_PASSWORD}} / {{BASIC_CREDENTIAL}}'],
      ['per-run temp root, repo root, os tmpdir, real HOME', '{{TMP}} / {{REPO}} / {{OS_TMPDIR}} / {{REAL_HOME}}']
    ].map(([value, placeholder]) => ({ value, placeholder })),
    // Scrub patterns are deliberately not echoed here: they contain the very strings goldens must never hold.
    scrubRules: SCRUB_RULES.map(r => ({ id: r.id, doc: r.doc, patternSource: 'scripts/golden/lib/scrub.mjs', flags: r.re.flags, placeholder: (r.to.match(/\{\{[A-Z_]+\}\}/) || [''])[0] })),
    normalizeRules: NORMALIZE_RULES.map(r => ({ id: r.id, doc: r.doc, pattern: r.re.source, flags: r.re.flags, to: r.to })),
    deterministicByConstruction: [
      'Stub ServiceNow generates sys_ids from a per-capture counter (sha-derived, not random) and a fixed server clock 2026-01-15 12:00:00; these are NOT normalized, so id/time handling drift is visible.',
      'JSON object keys sorted recursively; 2-space indent; trailing newline.',
      'JSON-RPC ids stripped from tool/CLI responses; surface keeps ids.'
    ],
    hitsPerStage: hits
  };
  return { manifest, normalization };
}

