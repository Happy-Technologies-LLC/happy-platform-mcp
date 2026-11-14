// Unit tests for the npm wrapper's launcher: node --test npm/test/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const binDir = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'happy-platform-mcp', 'bin');
const { detectLibc, packageId, resolveBinary } = require(path.join(binDir, 'launch.js'));
const isWin = process.platform === 'win32';

test('detectLibc: glibc when the report carries glibcVersionRuntime, musl otherwise', () => {
  assert.equal(detectLibc({ header: { glibcVersionRuntime: '2.39' } }), 'glibc');
  assert.equal(detectLibc({ header: {} }), 'musl');
  assert.equal(detectLibc(null), 'musl');
});

test('packageId maps hosts to the five platform packages', () => {
  assert.deepEqual(packageId('darwin', 'arm64'), { id: 'darwin-arm64' });
  assert.deepEqual(packageId('darwin', 'x64'), { id: 'darwin-x64' });
  assert.deepEqual(packageId('linux', 'x64', 'glibc'), { id: 'linux-x64-gnu' });
  assert.deepEqual(packageId('linux', 'arm64', 'glibc'), { id: 'linux-arm64-gnu' });
  assert.deepEqual(packageId('win32', 'x64'), { id: 'win32-x64' });
});

test('musl is refused with exit 127 and a message naming the libc', () => {
  const r = resolveBinary('happy-platform-mcp', {
    env: {},
    platform: 'linux',
    arch: 'x64',
    report: { header: {} }
  });
  assert.equal(r.code, 127);
  assert.match(r.error, /unsupported C library "musl"/);
  assert.match(r.error, /glibc/);
});

test('HPM_BINARY_PATH overrides resolution (also on musl)', () => {
  const r = resolveBinary('servicenow-mcp-server', {
    env: { HPM_BINARY_PATH: '/x/bin' },
    platform: 'linux',
    arch: 'x64',
    report: { header: {} }
  });
  assert.deepEqual(r, { bin: '/x/bin' });
});

test('unsupported os/arch gets its own message, exit 127', () => {
  for (const [platform, arch] of [['linux', 'ppc64'], ['win32', 'arm64'], ['freebsd', 'x64']]) {
    const r = resolveBinary('happy-platform-mcp', { env: {}, platform, arch, report: { header: { glibcVersionRuntime: '2.39' } } });
    assert.equal(r.code, 127);
    assert.match(r.error, new RegExp(`unsupported platform ${platform}/${arch}`));
    assert.doesNotMatch(r.error, /omit=optional/);
  }
});

test('installed platform package resolves bin/<name>(.exe)', () => {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'hpm-shim-')));
  const pkg = path.join(root, 'node_modules', '@happy-platform-mcp', 'win32-x64');
  fs.mkdirSync(path.join(pkg, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(pkg, 'package.json'), '{"name":"@happy-platform-mcp/win32-x64","version":"0.0.0"}');
  fs.writeFileSync(path.join(pkg, 'bin', 'servicenow-mcp-server.exe'), '');
  const r = resolveBinary('servicenow-mcp-server', { env: {}, platform: 'win32', arch: 'x64', from: root });
  assert.equal(r.bin, path.join(pkg, 'bin', 'servicenow-mcp-server.exe'));
  const miss = resolveBinary('happy-platform-mcp', { env: {}, platform: 'win32', arch: 'x64', from: root });
  assert.equal(miss.code, 127);
  assert.match(miss.error, /has no bin\/happy-platform-mcp\.exe/);
});

// Process-level: run the real bin entry with a fake native binary.
function runShim(binName, fakeScript, extraEnv = {}) {
  const env = { ...process.env, ...extraEnv };
  delete env.HPM_BINARY_PATH;
  if (fakeScript !== undefined) env.HPM_BINARY_PATH = fakeScript;
  return spawnSync(process.execPath, [path.join(binDir, `${binName}.js`), 'a b', '--flag'], {
    env,
    encoding: 'utf8',
    cwd: os.tmpdir()
  });
}

function fakeBinary(body) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'hpm-fake-'));
  const file = path.join(dir, 'fake.sh');
  fs.writeFileSync(file, `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  return file;
}

test('missing native package exits 127', () => {
  const r = runShim('happy-platform-mcp');
  assert.equal(r.status, 127);
  assert.match(r.stderr, /no native binary|unsupported C library/);
});

test('missing HPM_BINARY_PATH target exits 127', () => {
  const r = runShim('happy-platform-mcp', path.join(os.tmpdir(), 'hpm-does-not-exist'));
  assert.equal(r.status, 127);
  assert.match(r.stderr, /failed to launch/);
});

test('exit code and argv pass through', { skip: isWin }, () => {
  const fake = fakeBinary('printf "%s|" "$@"; exit 42');
  for (const name of ['happy-platform-mcp', 'servicenow-mcp-server', 'happy-platform-mcp-http']) {
    const r = runShim(name, fake);
    assert.equal(r.status, 42);
    assert.equal(r.stdout, 'a b|--flag|');
  }
});

test('child terminating signal is re-raised', { skip: isWin }, () => {
  const r = runShim('happy-platform-mcp', fakeBinary('kill -TERM $$'));
  assert.equal(r.signal, 'SIGTERM');
});

for (const sig of ['SIGPIPE', 'SIGUSR1']) {
  test(`child killed by ${sig} => wrapper exits 128+n, no inspector`, { skip: isWin }, () => {
    const r = runShim('happy-platform-mcp', fakeBinary(`kill -${sig.slice(3)} $$`));
    assert.equal(r.signal, null);
    assert.equal(r.status, 128 + os.constants.signals[sig]);
    assert.doesNotMatch(r.stderr, /Debugger listening/);
  });
}

test('SIGTERM to the wrapper is forwarded to the child', { skip: isWin }, async () => {
  const { spawn } = await import('node:child_process');
  const marker = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'hpm-sig-')), 'got');
  const fake = fakeBinary(`trap 'echo term > "${marker}"; exit 7' TERM; echo ready; while :; do sleep 0.05; done`);
  const child = spawn(process.execPath, [path.join(binDir, 'happy-platform-mcp.js')], {
    env: { ...process.env, HPM_BINARY_PATH: fake },
    stdio: ['ignore', 'pipe', 'inherit']
  });
  await new Promise((res) => child.stdout.once('data', res));
  child.kill('SIGTERM');
  const code = await new Promise((res) => child.on('exit', (c) => res(c)));
  assert.equal(code, 7);
  assert.equal(fs.readFileSync(marker, 'utf8').trim(), 'term');
});
