'use strict';
// Thin launcher shared by every bin of the `happy-platform-mcp` npm wrapper. It finds the
// per-platform package installed through optionalDependencies and runs the native binary with
// inherited stdio (the MCP stdio transport must pass through untouched), forwarding signals and
// passing the child's exit code or terminating signal through to the caller.
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const SCOPE = '@happy-platform-mcp';
const EXIT_NOT_FOUND = 127;
const EXIT_CANNOT_EXECUTE = 126;
const FORWARDED_SIGNALS = ['SIGINT', 'SIGTERM', 'SIGHUP'];

// glibcVersionRuntime is only present in Node's diagnostic report header on glibc hosts.
function detectLibc(report) {
  try {
    const r = report === undefined ? process.report.getReport() : report;
    return r && r.header && r.header.glibcVersionRuntime ? 'glibc' : 'musl';
  } catch {
    return 'glibc';
  }
}

// npm package id for a host, or an error string when the host is unsupported.
function packageId(platform, arch, libc) {
  if (platform === 'linux') {
    if (libc !== 'glibc') {
      return {
        error:
          `happy-platform-mcp: unsupported C library "${libc}" on linux/${arch}. ` +
          'Release 7 ships glibc builds only (x86_64/aarch64-unknown-linux-gnu); musl ' +
          '(e.g. Alpine) is not supported. Use a glibc-based distribution or the ' +
          'nczitzer/happy-platform-mcp container image, or set HPM_BINARY_PATH.'
      };
    }
    return { id: `linux-${arch}-gnu` };
  }
  return { id: `${platform}-${arch}` };
}

// Resolve the native binary for bin `name`. Returns { bin } or { error, code }.
function resolveBinary(name, ctx = {}) {
  const env = ctx.env || process.env;
  if (env.HPM_BINARY_PATH) return { bin: env.HPM_BINARY_PATH };
  const platform = ctx.platform || process.platform;
  const arch = ctx.arch || process.arch;
  const libc = platform === 'linux' ? detectLibc(ctx.report) : undefined;
  const p = packageId(platform, arch, libc);
  if (p.error) return { error: p.error, code: EXIT_NOT_FOUND };
  const pkg = `${SCOPE}/${p.id}`;
  const exe = platform === 'win32' ? `${name}.exe` : name;
  let why;
  try {
    const pkgJson = require.resolve(`${pkg}/package.json`, { paths: [ctx.from || __dirname] });
    const bin = path.join(path.dirname(pkgJson), 'bin', exe);
    if (fs.existsSync(bin)) return { bin };
    why = `${pkg} is installed but has no bin/${exe}`;
  } catch {
    why = `${pkg} is not installed`;
  }
  return {
    error:
      `happy-platform-mcp: no native binary for ${platform}/${arch}: ${why}.\n` +
      'Reinstall without --no-optional / --omit=optional, or set HPM_BINARY_PATH.',
    code: EXIT_NOT_FOUND
  };
}

function main(name) {
  const r = resolveBinary(name);
  if (r.error) {
    process.stderr.write(r.error + '\n');
    process.exit(r.code);
  }
  const child = spawn(r.bin, process.argv.slice(2), { stdio: 'inherit', windowsHide: true });
  const forward = (sig) => {
    if (child.exitCode === null && child.signalCode === null) child.kill(sig);
  };
  for (const sig of FORWARDED_SIGNALS) process.on(sig, forward);
  child.on('error', (err) => {
    process.stderr.write(`happy-platform-mcp: failed to launch ${r.bin}: ${err.message}\n`);
    process.exit(err.code === 'ENOENT' ? EXIT_NOT_FOUND : EXIT_CANNOT_EXECUTE);
  });
  child.on('exit', (code, signal) => {
    if (signal) {
      // Re-raise so the parent (MCP client) observes the same termination cause.
      for (const sig of FORWARDED_SIGNALS) process.removeListener(sig, forward);
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

module.exports = { detectLibc, packageId, resolveBinary, main, SCOPE };
