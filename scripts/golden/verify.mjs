#!/usr/bin/env node
// Re-capture into a temp dir and byte-compare with fixtures/golden. Exit 0 = identical, 1 = drift, 2 = harness failure.
//   node scripts/golden/verify.mjs [--golden <dir>] [--only surface,cli,tools] [--keep]
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { capture } from './capture.mjs';
import { DEFAULT_GOLDEN_OUT } from './lib/paths.mjs';
import { stableStringify } from './lib/scrub.mjs';
import { buildMeta } from './lib/meta.mjs';

function listAll(dir) {
  const out = new Map();
  const walk = (d, rel = '') => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const r = path.posix.join(rel, e.name);
      if (e.isDirectory()) walk(path.join(d, e.name), r);
      else out.set(r, fs.readFileSync(path.join(d, e.name), 'utf8'));
    }
  };
  walk(dir);
  return out;
}

async function main() {
  const argv = process.argv.slice(2);
  let golden = DEFAULT_GOLDEN_OUT;
  let only = ['surface', 'cli', 'tools'];
  let keep = false;
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--golden') golden = path.resolve(argv[++i]);
    else if (argv[i] === '--only') only = argv[++i].split(',');
    else if (argv[i] === '--keep') keep = true;
    else throw new Error(`unknown argument: ${argv[i]}`);
  }
  if (!fs.existsSync(golden)) throw new Error(`golden dir missing: ${golden}`);
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'hpm-golden-verify-'));
  const result = await capture({ out: tmp, only });
  const fresh = new Map(result.files);
  const { manifest, normalization } = buildMeta(result, { only });
  fresh.set('MANIFEST.json', stableStringify(manifest));
  fresh.set('NORMALIZATION.json', stableStringify(normalization));
  const existing = listAll(golden);
  const partial = only.length !== 3;
  const drift = [];
  for (const [rel, text] of fresh) {
    if (partial && (rel === 'MANIFEST.json' || rel === 'NORMALIZATION.json')) continue;
    if (!existing.has(rel)) drift.push(`NEW      ${rel}`);
    else if (existing.get(rel) !== text) drift.push(`CHANGED  ${rel}`);
  }
  for (const rel of existing.keys()) {
    if (partial && !only.some(s => rel.startsWith(`${s}/`))) continue;
    if (!fresh.has(rel)) drift.push(`MISSING  ${rel}`);
  }
  if (keep) {
    for (const [rel, text] of fresh) {
      const target = path.join(tmp, rel);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.writeFileSync(target, text);
    }
    process.stderr.write(`fresh capture kept at ${tmp}\n`);
  } else {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  if (drift.length) {
    process.stderr.write(`golden drift (${drift.length}):\n${drift.map(d => `  ${d}`).join('\n')}\n`);
    process.exit(1);
  }
  process.stdout.write(`golden verify OK: ${fresh.size} files byte-identical to ${golden}\n`);
}

main().catch(error => { process.stderr.write(`${error.stack || error}\n`); process.exit(2); });
