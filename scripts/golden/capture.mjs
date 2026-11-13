#!/usr/bin/env node
// Golden capture for the JS 6.2.0 baseline (plan §7.2).
//   node scripts/golden/capture.mjs [--out <dir>] [--only surface,cli,tools] [--force] [--tools-only <substr,...>]
// Default --out is fixtures/golden. Existing goldens are immutable: a non-empty --out is
// refused unless --force is given. Only loopback stubs are contacted (see preload.mjs).
import fs from 'node:fs';
import path from 'node:path';
import { createWorld, closeWorld, worldHits } from './lib/world.mjs';
import { stableStringify } from './lib/scrub.mjs';
import { buildMeta } from './lib/meta.mjs';
import { DEFAULT_GOLDEN_OUT } from './lib/paths.mjs';
import { runSurface } from './stages/mcp-surface.mjs';
import { runCli } from './stages/cli.mjs';
import { runTools } from './stages/tools.mjs';

const STAGES = {
  surface: { rules: 'scrub', run: (world) => runSurface(world) },
  cli: { rules: 'all', run: (world) => runCli(world) },
  tools: { rules: 'all', run: (world, opts) => runTools(world, { only: opts.toolsOnly }) }
};

function parseArgs(argv) {
  const opts = { out: DEFAULT_GOLDEN_OUT, only: Object.keys(STAGES), force: false, toolsOnly: undefined };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--out') opts.out = path.resolve(argv[++i]);
    else if (a === '--only') opts.only = argv[++i].split(',');
    else if (a === '--tools-only') opts.toolsOnly = argv[++i].split(',');
    else if (a === '--force') opts.force = true;
    else throw new Error(`unknown argument: ${a}`);
  }
  for (const s of opts.only) if (!STAGES[s]) throw new Error(`unknown stage: ${s}`);
  return opts;
}

export async function capture(opts) {
  const files = new Map();
  const hits = {};
  for (const stage of opts.only) {
    const world = await createWorld({ scrubRules: STAGES[stage].rules });
    try {
      const out = await STAGES[stage].run(world, opts);
      for (const [rel, doc] of out) files.set(rel, stableStringify(doc));
      hits[stage] = worldHits(world);
    } finally {
      await closeWorld(world);
    }
  }
  return { files, hits };
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const result = await capture(opts);
  if (fs.existsSync(opts.out) && fs.readdirSync(opts.out).length) {
    if (!opts.force) throw new Error(`${opts.out} is not empty; goldens are immutable (use --force to replace)`);
    fs.rmSync(opts.out, { recursive: true, force: true });
  }
  const { manifest, normalization } = buildMeta(result, opts);
  const all = new Map(result.files);
  all.set('MANIFEST.json', stableStringify(manifest));
  all.set('NORMALIZATION.json', stableStringify(normalization));
  for (const [rel, text] of all) {
    const target = path.join(opts.out, rel);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, text);
  }
  const c = manifest.coverage;
  process.stdout.write(`captured ${result.files.size} files -> ${opts.out}\ncoverage: ${c.toolsWithAtLeastOneCapture}/${c.toolsServed} tools with >=1 capture; ${c.toolsWithAtLeastOneOutboundRequest} with >=1 outbound request\n`);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch(error => { process.stderr.write(`${error.stack || error}\n`); process.exit(1); });
}
