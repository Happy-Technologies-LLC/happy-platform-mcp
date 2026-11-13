// Per-capture "world": stub ServiceNow + raw-GitHub stub, isolated temp dirs,
// and the scrubber literals that map those ephemeral values to placeholders.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { startStub, startRawStub } from './stub-sn.mjs';
import { Scrubber } from './scrub.mjs';
import { REPO } from './paths.mjs';

export const FAKE_PASSWORD = 'golden-fake-password-4f9a1c';
export const FAKE_USER = 'golden.user';

export const DEFAULT_DOCS_ROUTES = {
  '/ServiceNow/ServiceNowDocs/main/llms.txt': [
    '# ServiceNow Product Documentation', '',
    '- Family-to-branch mapping is:',
    '    - "australia" : "australia" -- australia family', ''
  ].join('\n'),
  '/ServiceNow/ServiceNowDocs/australia/llms.txt': '- [Flow](platform/flow.md)\n- [Tables](platform/tables.md)\n',
  '/ServiceNow/ServiceNowDocs/australia/platform/flow.md': '# Flow\n\nCreate flow actions and subflows.\n',
  '/ServiceNow/ServiceNowDocs/australia/platform/tables.md': '# Tables\n\nTables store records.\n'
};

export async function createWorld({ scrubRules = 'all' } = {}) {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'hpm-golden-')));
  const stub = await startStub({ host: '127.0.0.1' });
  const raw = await startRawStub(DEFAULT_DOCS_ROUTES);
  const world = { root, stub, raw, scrubRules, scrubber: null, hits: new Map() };
  world.scrubber = new Scrubber(literalsFor(world), { rules: scrubRules });
  return world;
}

export function literalsFor(world) {
  const { root, stub, raw } = world;
  const lits = [
    [stub.origin, '{{INSTANCE_URL}}'],
    [`http://localhost:${stub.port}`, '{{INSTANCE_URL_ALT}}'],
    [`127.0.0.1:${stub.port}`, '{{INSTANCE_HOST}}'],
    [`localhost:${stub.port}`, '{{INSTANCE_HOST_ALT}}'],
    [raw.base, '{{RAW_GITHUB_URL}}'],
    [`127.0.0.1:${raw.port}`, '{{RAW_GITHUB_HOST}}'],
    [FAKE_PASSWORD, '{{FAKE_PASSWORD}}'],
    [Buffer.from(`${FAKE_USER}:${FAKE_PASSWORD}`).toString('base64'), '{{BASIC_CREDENTIAL}}'],
    [root, '{{TMP}}'],
    [REPO, '{{REPO}}'],
    [os.tmpdir(), '{{OS_TMPDIR}}'],
    [os.homedir(), '{{REAL_HOME}}']
  ];
  try { lits.push([fs.realpathSync(REPO), '{{REPO}}']); } catch { /* ignore */ }
  return lits;
}

function foldHits(world) {
  for (const [id, n] of world.scrubber.hits) world.hits.set(id, (world.hits.get(id) || 0) + n);
  world.scrubber.hits.clear();
}

/** Scrub/normalize hit counts for the whole world (all scrubbers so far). */
export function worldHits(world) {
  foldHits(world);
  return Object.fromEntries([...world.hits.entries()].sort(([a], [b]) => (a < b ? -1 : 1)));
}

export function refreshScrubber(world) {
  foldHits(world);
  world.scrubber = new Scrubber(literalsFor(world), { rules: world.scrubRules });
}

export async function closeWorld(world) {
  await world.stub.close();
  await world.raw.close();
  fs.rmSync(world.root, { recursive: true, force: true });
}

/** Fresh sandbox for one server process: HOME, config, cwd, cache dirs. */
export function makeSandbox(world, name, { instances = null, env: extraEnv = {}, withCreds = true } = {}) {
  const dir = path.join(world.root, name.replace(/[^A-Za-z0-9_.-]/g, '_'));
  const home = path.join(dir, 'home');
  const work = path.join(dir, 'work');
  const cfgDir = path.join(dir, 'cfg');
  for (const d of [home, work, cfgDir, path.join(dir, 'xdg'), path.join(dir, 'docs-cache')]) fs.mkdirSync(d, { recursive: true });
  const configPath = path.join(cfgDir, 'instances.json');
  if (instances) fs.writeFileSync(configPath, JSON.stringify(instances(world), null, 2));
  const env = {
    PATH: process.env.PATH,
    HOME: home,
    USERPROFILE: home,
    TMPDIR: path.join(dir, 'tmp'),
    XDG_CONFIG_HOME: path.join(dir, 'xdg'),
    TZ: 'UTC',
    LANG: 'C.UTF-8',
    NO_COLOR: '1',
    HAPPY_CONFIG_PATH: configPath,
    HAPPY_DOCS_CACHE_DIR: path.join(dir, 'docs-cache'),
    GOLDEN_KEYRING_FILE: path.join(dir, 'keyring.json'),
    GOLDEN_RAW_GITHUB_BASE: world.raw.base,
    SERVICENOW_TOKEN_STORE: 'file',
    ...(withCreds && !instances ? {
      SERVICENOW_INSTANCE_URL: world.stub.origin,
      SERVICENOW_USERNAME: FAKE_USER,
      SERVICENOW_PASSWORD: FAKE_PASSWORD
    } : {}),
    ...extraEnv
  };
  fs.mkdirSync(env.TMPDIR, { recursive: true });
  return { dir, home, work, configPath, env };
}

/** Files created under `work/` (fix scripts) as { relPath: content }. */
export function listFiles(dir) {
  const out = {};
  const walk = (d, rel = '') => {
    if (!fs.existsSync(d)) return;
    for (const entry of fs.readdirSync(d, { withFileTypes: true }).sort((a, b) => (a.name < b.name ? -1 : 1))) {
      const r = path.posix.join(rel, entry.name);
      if (entry.isDirectory()) walk(path.join(d, entry.name), r);
      else out[r] = fs.readFileSync(path.join(d, entry.name), 'utf8');
    }
  };
  walk(dir);
  return out;
}
