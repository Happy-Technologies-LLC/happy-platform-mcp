#!/usr/bin/env node
// Assemble the npm wrapper + per-platform packages from a release run's dist archives.
//
//   node scripts/release/assemble-npm.mjs --archives DIR --version X.Y.Z[-pre] --out DIR
//
// DIR must hold, for EVERY target in npm/platforms.json, `happy-platform-mcp-<target>.tar.xz`
// (`.zip` for Windows) and its `.sha256` sidecar. Each archive is checked against its sidecar
// before extraction; a missing target, a checksum mismatch or a missing binary aborts with no
// output left behind. Output: OUT/happy-platform-mcp (wrapper) and OUT/<platform-id> each, ready
// for `npm pack` / `npm publish`. Nothing is built here: binaries come only from the archives.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const npmDir = path.join(repo, 'npm');
const { scope, binaries, platforms } = JSON.parse(
  fs.readFileSync(path.join(npmDir, 'platforms.json'), 'utf8')
);
const WRAPPER = 'happy-platform-mcp';

function fail(msg) {
  console.error(`assemble-npm: ${msg}`);
  process.exit(1);
}

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i < 0 ? undefined : args[i + 1];
};
const archives = flag('--archives');
const out = flag('--out');
const version = flag('--version')?.replace(/^v/, '');
if (!archives || !out || !version) {
  console.error('usage: assemble-npm.mjs --archives DIR --version X.Y.Z --out DIR');
  process.exit(2);
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) fail(`not a semver version: ${version}`);

const archiveName = (p) => `${WRAPPER}-${p.rust}${p.os === 'win32' ? '.zip' : '.tar.xz'}`;

// 1. Every target present and matching its checksum, before anything is written.
const missing = platforms.flatMap((p) =>
  [archiveName(p), `${archiveName(p)}.sha256`].filter((f) => !fs.existsSync(path.join(archives, f)))
);
if (missing.length) fail(`missing release files in ${archives}: ${missing.join(', ')}`);
for (const p of platforms) {
  const file = path.join(archives, archiveName(p));
  // dist writes `<hex> *<name>`; also accept `<hex>  <name>` and a bare hex digest.
  const expected = fs.readFileSync(`${file}.sha256`, 'utf8').trim().split(/\s+/)[0].toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(expected)) fail(`${archiveName(p)}.sha256 holds no sha256 digest`);
  const actual = createHash('sha256').update(fs.readFileSync(file)).digest('hex');
  if (actual !== expected) fail(`sha256 mismatch for ${archiveName(p)}: expected ${expected}, got ${actual}`);
  console.log(`verified ${archiveName(p)} sha256 ${actual}`);
}

// Recursively find a regular file called `name` (dist nests binaries in `<archive-stem>/`).
function findFile(dir, name) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, e.name);
    if (e.isFile() && e.name === name) return full;
    if (e.isDirectory()) {
      const hit = findFile(full, name);
      if (hit) return hit;
    }
  }
  return undefined;
}

function extract(file, dest) {
  fs.mkdirSync(dest, { recursive: true });
  if (file.endsWith('.zip') && process.platform === 'linux') {
    execFileSync('unzip', ['-q', file, '-d', dest], { stdio: 'inherit' });
  } else {
    // bsdtar (macOS, Windows) reads both .tar.xz and .zip; GNU tar reads .tar.xz.
    execFileSync('tar', ['-xf', file, '-C', dest], { stdio: 'inherit' });
  }
}

const copyLegal = (dir, names) => {
  for (const n of names) fs.copyFileSync(path.join(repo, n), path.join(dir, n));
};

const work = fs.mkdtempSync(path.join(os.tmpdir(), 'hpm-assemble-'));
const staged = path.join(work, 'out');
try {
  // 2. Platform packages: all binaries of the target, exact os/cpu/libc.
  for (const p of platforms) {
    const src = path.join(work, p.id);
    extract(path.join(archives, archiveName(p)), src);
    const dir = path.join(staged, p.id);
    fs.mkdirSync(path.join(dir, 'bin'), { recursive: true });
    for (const b of binaries) {
      const exe = p.os === 'win32' ? `${b}.exe` : b;
      const found = findFile(src, exe);
      if (!found) fail(`${archiveName(p)} has no ${exe}`);
      fs.copyFileSync(found, path.join(dir, 'bin', exe));
      fs.chmodSync(path.join(dir, 'bin', exe), 0o755);
    }
    copyLegal(dir, ['LICENSE', 'NOTICE']);
    const pkg = {
      name: `${scope}/${p.id}`,
      version,
      description: `Native happy-platform-mcp binaries for ${p.rust}. Install happy-platform-mcp instead.`,
      license: 'Apache-2.0',
      repository: {
        type: 'git',
        url: 'git+https://github.com/Happy-Technologies-LLC/happy-platform-mcp.git'
      },
      os: [p.os],
      cpu: [p.cpu],
      ...(p.libc ? { libc: [p.libc] } : {}),
      files: ['bin/', 'LICENSE', 'NOTICE'],
      preferUnplugged: true
    };
    fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
    console.log(`assembled ${pkg.name}@${version} (${binaries.length} binaries)`);
  }

  // 3. Wrapper: exact-pinned optionalDependencies on every platform package.
  const wdir = path.join(staged, WRAPPER);
  fs.cpSync(path.join(npmDir, WRAPPER), wdir, { recursive: true });
  const main = JSON.parse(fs.readFileSync(path.join(wdir, 'package.json'), 'utf8'));
  main.version = version;
  main.optionalDependencies = Object.fromEntries(platforms.map((p) => [`${scope}/${p.id}`, version]));
  fs.writeFileSync(path.join(wdir, 'package.json'), JSON.stringify(main, null, 2) + '\n');
  copyLegal(wdir, ['README.md', 'LICENSE', 'NOTICE']);
  console.log(`assembled ${WRAPPER}@${version} (optionalDependencies: ${platforms.length})`);

  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(path.resolve(out)), { recursive: true });
  fs.cpSync(staged, out, { recursive: true });
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
