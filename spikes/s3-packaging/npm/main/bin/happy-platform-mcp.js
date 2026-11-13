#!/usr/bin/env node
'use strict';
// Thin launcher: finds the per-platform package installed via optionalDependencies and execs
// its native binary with inherited stdio (MCP stdio transport must pass through untouched).
// Both bin names (happy-platform-mcp, servicenow-mcp-server) point at this one file.
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const SCOPE = '@happy-platform-mcp';

function isMusl() {
  // glibcVersionRuntime is only present in the diagnostic report on glibc.
  try {
    const header = process.report && process.report.getReport().header;
    return !(header && header.glibcVersionRuntime);
  } catch {
    return false;
  }
}

function candidateIds() {
  const { platform, arch } = process;
  if (platform === 'linux') {
    const libcs = isMusl() ? ['musl', 'gnu'] : ['gnu', 'musl']; // static musl binary runs on glibc hosts too
    return libcs.map((libc) => `linux-${arch}-${libc}`);
  }
  return [`${platform}-${arch}`];
}

function resolveBinary() {
  if (process.env.HPM_BINARY_PATH) return process.env.HPM_BINARY_PATH; // dev / debugging override
  const exe = process.platform === 'win32' ? 'happy-platform-mcp.exe' : 'happy-platform-mcp';
  const tried = [];
  for (const id of candidateIds()) {
    const pkg = `${SCOPE}/${id}`;
    try {
      const pkgJson = require.resolve(`${pkg}/package.json`, { paths: [__dirname] });
      const bin = path.join(path.dirname(pkgJson), 'bin', exe);
      if (fs.existsSync(bin)) return bin;
      tried.push(`${pkg} (installed, binary missing)`);
    } catch {
      tried.push(`${pkg} (not installed)`);
    }
  }
  process.stderr.write(
    `happy-platform-mcp: no native binary for ${process.platform}/${process.arch}.\n` +
      `Tried: ${tried.join(', ')}.\n` +
      'Reinstall without --no-optional / --omit=optional, or set HPM_BINARY_PATH.\n'
  );
  process.exit(127);
}

const bin = resolveBinary();
const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit', windowsHide: true });

for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(sig, () => {
    if (!child.killed) child.kill(sig);
  });
}
child.on('error', (err) => {
  process.stderr.write(`happy-platform-mcp: failed to launch ${bin}: ${err.message}\n`);
  process.exit(126);
});
child.on('exit', (code, signal) => {
  if (signal) {
    // Re-raise so the parent (MCP client) sees the same termination cause.
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
