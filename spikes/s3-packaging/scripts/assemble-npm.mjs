#!/usr/bin/env node
// Assemble npm packages from native binaries.
//   node scripts/assemble-npm.mjs --version X --out DIR --bin <rust-triple>=<path-to-binary> [...]
// Produces DIR/main (wrapper) and DIR/<platform-id> for each --bin. The main package's
// optionalDependencies always list ALL platforms (pinned to the exact version) so that a
// full release is consistent; pass only the binaries you have (local test = host only).
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const npmDir = path.join(here, '..', 'npm');
const { scope, platforms } = JSON.parse(fs.readFileSync(path.join(npmDir, 'platforms.json'), 'utf8'));

const args = process.argv.slice(2);
const flag = (name) => { const i = args.indexOf(name); return i < 0 ? undefined : args[i + 1]; };
const version = flag('--version');
const out = flag('--out');
const bins = new Map();
args.forEach((a, i) => { if (a === '--bin') { const [t, p] = args[i + 1].split(/=(.*)/s); bins.set(t, p); } });
if (!version || !out || bins.size === 0) {
  console.error('usage: assemble-npm.mjs --version X --out DIR --bin <rust-triple>=<binary> ...');
  process.exit(2);
}

fs.rmSync(out, { recursive: true, force: true });
const main = JSON.parse(fs.readFileSync(path.join(npmDir, 'main', 'package.json'), 'utf8'));
main.version = version;
main.optionalDependencies = Object.fromEntries(platforms.map((p) => [`${scope}/${p.id}`, version]));
fs.cpSync(path.join(npmDir, 'main'), path.join(out, 'main'), { recursive: true });
fs.writeFileSync(path.join(out, 'main', 'package.json'), JSON.stringify(main, null, 2) + '\n');

for (const p of platforms) {
  const src = bins.get(p.rust);
  if (!src) continue;
  const exe = p.os === 'win32' ? 'happy-platform-mcp.exe' : 'happy-platform-mcp';
  const dir = path.join(out, p.id);
  fs.mkdirSync(path.join(dir, 'bin'), { recursive: true });
  fs.copyFileSync(src, path.join(dir, 'bin', exe));
  fs.chmodSync(path.join(dir, 'bin', exe), 0o755);
  const pkg = {
    name: `${scope}/${p.id}`,
    version,
    description: `Native happy-platform-mcp binary for ${p.rust}`,
    license: 'Apache-2.0',
    os: [p.os],
    cpu: [p.cpu],
    ...(p.libc ? { libc: [p.libc] } : {}),
    files: ['bin/'],
    preferUnplugged: true
  };
  fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
  console.log(`assembled ${pkg.name}@${version} <- ${src}`);
}
console.log(`assembled main happy-platform-mcp@${version} (optionalDependencies: ${platforms.length})`);
