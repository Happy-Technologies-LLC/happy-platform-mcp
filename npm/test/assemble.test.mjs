// Tests for scripts/release/assemble-npm.mjs with fake dist archives: node --test npm/test/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const script = path.join(repo, 'scripts', 'release', 'assemble-npm.mjs');
const { scope, binaries, platforms } = JSON.parse(fs.readFileSync(path.join(repo, 'npm', 'platforms.json'), 'utf8'));
const VERSION = '7.0.0-test.1';
// publish-npm.yml assembles on Linux; Windows' bundled tar is not relied on for .tar.xz.
const skip = process.platform === 'win32' ? 'assembly runs on Linux/macOS only' : false;

const archiveName = (p) => `happy-platform-mcp-${p.rust}${p.os === 'win32' ? '.zip' : '.tar.xz'}`;

// Build every target's archive (bsdtar/GNU tar for .tar.xz; bsdtar or zip for .zip) + `<hex> *<name>` sidecar.
function fakeArchives() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'hpm-assemble-test-'));
  const dir = path.join(root, 'archives');
  fs.mkdirSync(dir);
  for (const p of platforms) {
    const stem = `happy-platform-mcp-${p.rust}`;
    const stage = path.join(root, 'stage', stem);
    fs.mkdirSync(stage, { recursive: true });
    for (const b of binaries) {
      fs.writeFileSync(path.join(stage, p.os === 'win32' ? `${b}.exe` : b), `fake ${b} for ${p.rust}\n`);
    }
    const name = archiveName(p);
    const out = path.join(dir, name);
    const cwd = path.join(root, 'stage');
    if (name.endsWith('.zip')) {
      if (process.platform === 'linux') execFileSync('zip', ['-qr', out, stem], { cwd });
      else execFileSync('tar', ['-a', '-cf', out, stem], { cwd });
    } else {
      execFileSync('tar', ['-cJf', out, stem], { cwd });
    }
    const hex = createHash('sha256').update(fs.readFileSync(out)).digest('hex');
    fs.writeFileSync(`${out}.sha256`, `${hex} *${name}\n`);
  }
  return { root, dir };
}

const assemble = (archives, out) =>
  spawnSync(process.execPath, [script, '--archives', archives, '--version', `v${VERSION}`, '--out', out], {
    encoding: 'utf8'
  });

test('assembles the wrapper and every platform package', { skip }, () => {
  const { root, dir } = fakeArchives();
  const out = path.join(root, 'out');
  const r = assemble(dir, out);
  assert.equal(r.status, 0, r.stderr);
  const main = JSON.parse(fs.readFileSync(path.join(out, 'happy-platform-mcp', 'package.json'), 'utf8'));
  assert.equal(main.version, VERSION);
  assert.deepEqual(
    main.optionalDependencies,
    Object.fromEntries(platforms.map((p) => [`${scope}/${p.id}`, VERSION]))
  );
  assert.deepEqual(Object.keys(main.bin).sort(), [...binaries].sort());
  for (const p of platforms) {
    const pkg = JSON.parse(fs.readFileSync(path.join(out, p.id, 'package.json'), 'utf8'));
    assert.equal(pkg.version, VERSION);
    assert.deepEqual(pkg.os, [p.os]);
    assert.deepEqual(pkg.cpu, [p.cpu]);
    assert.deepEqual(pkg.libc, p.libc ? [p.libc] : undefined);
    for (const b of binaries) {
      const exe = p.os === 'win32' ? `${b}.exe` : b;
      assert.equal(fs.readFileSync(path.join(out, p.id, 'bin', exe), 'utf8'), `fake ${b} for ${p.rust}\n`);
    }
  }
});

test('refuses a tampered archive (sha256 mismatch) and writes nothing', { skip }, () => {
  const { root, dir } = fakeArchives();
  const victim = path.join(dir, archiveName(platforms[1]));
  fs.appendFileSync(victim, 'tampered');
  const out = path.join(root, 'out');
  const r = assemble(dir, out);
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /sha256 mismatch/);
  assert.equal(fs.existsSync(out), false);
});

test('refuses when a target is missing and writes nothing', { skip }, () => {
  const { root, dir } = fakeArchives();
  fs.rmSync(path.join(dir, archiveName(platforms[4])));
  const out = path.join(root, 'out');
  const r = assemble(dir, out);
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /missing release files/);
  assert.equal(fs.existsSync(out), false);
});
