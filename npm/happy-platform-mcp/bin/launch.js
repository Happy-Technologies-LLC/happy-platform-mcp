'use strict';
// Thin launcher shared by every bin of the `happy-platform-mcp` npm wrapper. It finds the
// per-platform package installed through optionalDependencies and runs the native binary with
// inherited stdio (the MCP stdio transport must pass through untouched), forwarding signals and
// passing the child's exit code or terminating signal through to the caller.
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

// Platform package name = PACKAGE_PREFIX + id (npm/platforms.json "packagePrefix").
const PACKAGE_PREFIX = '@happy-technologies/happy-platform-mcp-';
const EXIT_NOT_FOUND = 127;
const EXIT_CANNOT_EXECUTE = 126;
const FORWARDED_SIGNALS = ['SIGINT', 'SIGTERM', 'SIGHUP'];
// Signals whose default action terminates Node, so re-raising reproduces the child's death.
// Others (SIGPIPE is ignored by Node, SIGUSR1 starts the inspector) are mapped to 128+n.
const RERAISE_SIGNALS = new Set(['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT', 'SIGABRT', 'SIGKILL']);
// Hosts with a published platform package (npm/platforms.json).
const SUPPORTED = new Set(['darwin-arm64', 'darwin-x64', 'linux-x64', 'linux-arm64', 'win32-x64']);

// glibcVersionRuntime is only present in Node's diagnostic report header on glibc hosts.
function detectLibc(report) {
  try {
    let r = report;
    if (r === undefined) {
      process.report.excludeNetwork = true; // network enumeration can take seconds
      r = process.report.getReport();
    }
    return r && r.header && r.header.glibcVersionRuntime ? 'glibc' : 'musl';
  } catch {
    return 'glibc';
  }
}

// npm package id for a host, or an error string when the host is unsupported.
function packageId(platform, arch, libc) {
  if (!SUPPORTED.has(`${platform}-${arch}`)) {
    return {
      error:
        `happy-platform-mcp: unsupported platform ${platform}/${arch}. Native builds exist for ` +
        'darwin/arm64, darwin/x64, linux/x64, linux/arm64 (glibc) and win32/x64. ' +
        'Build from source and set HPM_BINARY_PATH, or use the container image.'
    };
  }
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
  const pkg = `${PACKAGE_PREFIX}${p.id}`;
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
      const status = 128 + (os.constants.signals[signal] || 0);
      if (RERAISE_SIGNALS.has(signal)) {
        // Re-raise so the parent (MCP client) observes the same termination cause.
        for (const sig of FORWARDED_SIGNALS) process.removeListener(sig, forward);
        try {
          process.kill(process.pid, signal);
        } catch {
          // fall through to the exit status below
        }
        // Still alive (signal not delivered synchronously or handled): exit like a shell would.
        setTimeout(() => process.exit(status), 100);
        return;
      }
      process.exit(status);
    }
    process.exit(code === null ? 1 : code);
  });
}

module.exports = { detectLibc, packageId, resolveBinary, main, PACKAGE_PREFIX };
