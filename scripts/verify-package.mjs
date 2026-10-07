import { createHash, randomBytes } from 'node:crypto';
import { lstat, mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer, request as httpRequest } from 'node:http';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { inspect } from 'node:util';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const sdkManifestPath = join(root, 'node_modules', '@modelcontextprotocol', 'sdk', 'package.json');
const initializeSessionId = 'package-verifier-session';
const initializeProtocolVersion = '2025-06-18';
const initializeClientInfo = {
  name: 'happy-platform-mcp-package-verifier',
  version: '1.0.0',
};
const initializeServerInfo = {
  name: 'happy-platform-mcp-package-smoke',
  version: '1.0.0',
};
const cleanupTimeoutMs = 10_000;
const initializeTimeoutMs = 15_000;
const listenerTimeoutMs = 10_000;
const npmLocalTimeoutMs = 2 * 60_000;
const npmNetworkTimeoutMs = 10 * 60_000;
const workspaceCleanupTimeoutMs = 30_000;
export const initializeResponseMaxBytes = 64 * 1_024;
export const subprocessDiagnosticMaxBytes = 64 * 1_024;
export const subprocessMaxBufferBytes = 16 * 1_024 * 1_024;

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

export function parsePackOutput(output) {
  let parsed;
  try { parsed = JSON.parse(output); } catch (cause) { throw new Error('npm pack returned invalid JSON', { cause }); }
  const records = Array.isArray(parsed) ? parsed : parsed && typeof parsed === 'object' ? Object.values(parsed) : [];
  assert(records.length === 1, 'npm pack must report exactly one tarball');
  const pack = records[0];
  assert(pack && typeof pack.filename === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._-]*\.tgz$/.test(pack.filename), 'npm pack reported an unsafe tarball filename');
  assert(Array.isArray(pack.files) && pack.files.length > 0 && pack.files.every((file) => file && typeof file.path === 'string'), 'npm pack did not report valid tarball files');
  return pack;
}

const allowedRootFiles = new Set([
  'package.json', 'README.md', 'CHANGELOG.md', 'SECURITY.md', 'LICENSE', 'NOTICE', 'server.json',
  'config/servicenow-instances.json.example', 'assets/logo.svg', 'assets/logo-400x400.png',
  'src/config/comprehensive-table-definitions.json', 'src/config/table-definitions.json', 'src/config/prompts.md',
  'scripts/verify-package.mjs',
]);

export function assertPackageFiles(files, bundledPaths = ['node_modules/@modelcontextprotocol/sdk', 'node_modules/@hono/node-server']) {
  for (const { path } of files) {
    assert(typeof path === 'string' && path.length > 0 && !path.includes('\\') && !path.startsWith('/') &&
      path.split('/').every((segment) => segment && segment !== '.' && segment !== '..'), 'packed tarball contains an unsafe path');
    assert(!path.split('/').some((segment) => segment.startsWith('.env') || segment.startsWith('.mcpregistry_') ||
      segment === '.cache' || segment.includes('.backup') || segment.startsWith('.happy-platform-mcp-sdk-manifest-') ||
      /\.(?:pem|key|p12|pfx|log|tmp|sqlite|db)$/i.test(segment)), 'packed tarball contains secret/cache/backup/temp files');
    assert(allowedRootFiles.has(path) || /^src\/(?:[a-zA-Z0-9_-]+\/)*[a-zA-Z0-9_-]+\.js$/.test(path) ||
      /^docs\/[a-zA-Z0-9_-]+\.md$/.test(path) || bundledPaths.some((directory) =>
        path.startsWith(directory + '/') && !path.slice(directory.length + 1).includes('node_modules/')),
      'packed tarball contains a file outside the runtime allow-list: ' + path);
  }
}

function assertPositiveSafeInteger(value, label) {
  assert(Number.isSafeInteger(value) && value > 0, `${label} must be a positive safe integer`);
}

function truncateDiagnostic(value, maxBytes) {
  const bytes = Buffer.from(value);
  if (bytes.length <= maxBytes) {
    return value;
  }

  const marker = '\n... [diagnostic truncated] ...\n';
  const markerBytes = Buffer.byteLength(marker);
  const retainedBytes = maxBytes - markerBytes;
  const headBytes = Math.ceil(retainedBytes / 2);
  const tailBytes = Math.floor(retainedBytes / 2);
  return `${bytes.subarray(0, headBytes).toString('utf8')}${marker}${bytes.subarray(
    bytes.length - tailBytes,
  ).toString('utf8')}`;
}

function renderSubprocessDiagnostics(stdout, stderr) {
  const entries = [
    ['stdout', stdout],
    ['stderr', stderr],
  ].filter(([, value]) => typeof value === 'string' && value.length > 0);
  if (entries.length === 0) {
    return '';
  }

  const perStreamMaxBytes = Math.floor(subprocessDiagnosticMaxBytes / entries.length);
  return entries
    .map(([label, value]) => `[${label}]\n${truncateDiagnostic(value, perStreamMaxBytes)}`)
    .join('\n');
}

export function normalizeThrownValue(value, context) {
  if (value instanceof Error) {
    return value;
  }
  return new Error(`${context} threw or rejected with a non-Error value: ${inspect(value)}`, {
    cause: value,
  });
}

function throwCollectedErrors(primaryFailed, primaryError, cleanupErrors, message) {
  if (primaryFailed) {
    const normalizedPrimaryError = normalizeThrownValue(primaryError, 'package verifier');
    if (cleanupErrors.length > 0) {
      throw new AggregateError(
        [
          normalizedPrimaryError,
          ...cleanupErrors.map((error) => normalizeThrownValue(error, 'package verifier cleanup')),
        ],
        message,
      );
    }
    throw normalizedPrimaryError;
  }
  if (cleanupErrors.length > 0) {
    throw new AggregateError(
      cleanupErrors.map((error) => normalizeThrownValue(error, 'package verifier cleanup')),
      message,
    );
  }
}

async function withDeadline(operation, timeoutMs, message) {
  let timer;
  try {
    return await Promise.race([
      Promise.resolve().then(operation),
      new Promise((_, reject) => {
        timer = setTimeout(() => {
          reject(new Error(`${message} timed out after ${timeoutMs} ms`));
        }, timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function validatedNpmCliPath() {
  const configuredPath = process.env.npm_execpath;
  assert(
    typeof configuredPath === 'string' && configuredPath.length > 0,
    'npm_execpath is unavailable; run this verifier through npm run test:package',
  );

  const cliPath = resolve(configuredPath);
  let metadata;
  try {
    metadata = await lstat(cliPath);
  } catch (error) {
    throw new Error(`npm_execpath does not identify a readable npm CLI file: ${cliPath}`, {
      cause: error,
    });
  }
  assert(
    metadata.isFile() && !metadata.isSymbolicLink(),
    `npm_execpath must identify a regular npm CLI JavaScript file: ${cliPath}`,
  );
  return cliPath;
}

export function run(
  command,
  args,
  cwd = root,
  timeoutMs = npmLocalTimeoutMs,
  maxBufferBytes = subprocessMaxBufferBytes,
) {
  assertPositiveSafeInteger(maxBufferBytes, 'subprocess output byte limit');
  const invocation = `${command} ${args.join(' ')}`;
  const result = spawnSync(command, args, {
    cwd,
    encoding: 'utf8',
    env: { ...process.env, npm_config_ignore_scripts: 'false' },
    killSignal: 'SIGKILL',
    maxBuffer: maxBufferBytes,
    timeout: timeoutMs,
  });
  const termination = `status ${result.status ?? 'null'}, signal ${result.signal ?? 'none'}`;
  const diagnostics = renderSubprocessDiagnostics(result.stdout, result.stderr);
  const diagnosticSuffix = diagnostics ? `\n${diagnostics}` : '';

  if (result.error) {
    let message;
    if (result.error.code === 'ENOBUFS') {
      message = `${invocation} exceeded subprocess output limit of ${maxBufferBytes} bytes (${termination})`;
    } else if (result.error.code === 'ETIMEDOUT') {
      message = `${invocation} timed out after ${timeoutMs} ms and was sent SIGKILL (${termination})`;
    } else {
      message = `${invocation} could not be started (${termination})`;
    }
    throw new Error(`${message}${diagnosticSuffix}`, { cause: result.error });
  }
  if (result.status !== 0) {
    throw new Error(`${invocation} failed (${termination})${diagnosticSuffix}`);
  }

  return result.stdout.trim();
}

function runNpm(npmCliPath, args, cwd = root) {
  const timeoutMs = ['audit', 'install', 'pack'].includes(args[0])
    ? npmNetworkTimeoutMs
    : npmLocalTimeoutMs;
  return run(process.execPath, [npmCliPath, ...args], cwd, timeoutMs);
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function versionsFor(tree, packageName, versions = []) {
  for (const [name, dependency] of Object.entries(tree.dependencies ?? {})) {
    if (name === packageName) {
      versions.push(dependency.version);
    }
    versionsFor(dependency, packageName, versions);
  }
  return versions;
}

function requireOnlyVersion(tree, packageName, expectedVersion) {
  const versions = versionsFor(tree, packageName);
  assert(versions.length > 0, `${packageName} is missing from the consumer tree`);
  assert(
    versions.every((version) => version === expectedVersion),
    `${packageName} resolved unexpected versions: ${versions.join(', ')}`,
  );
}

export function listenOnLoopback(listener, timeoutMs = listenerTimeoutMs) {
  return new Promise((resolvePromise, reject) => {
    let settled = false;
    const finish = (callback, value) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      listener.off('error', onError);
      listener.off('listening', onListening);
      callback(value);
    };
    const onError = (error) => finish(reject, error);
    const onListening = () => {
      const address = listener.address();
      if (!address || typeof address !== 'object') {
        finish(reject, new Error('package verifier listener has no TCP address'));
        return;
      }
      finish(resolvePromise, address.port);
    };
    const timer = setTimeout(() => {
      finish(
        reject,
        new Error(`package verifier listener startup timed out after ${timeoutMs} ms`),
      );
    }, timeoutMs);

    listener.once('error', onError);
    listener.once('listening', onListening);
    try {
      listener.listen(0, '127.0.0.1');
    } catch (error) {
      finish(reject, error);
    }
  });
}

export function closeListener(listener, timeoutMs = listenerTimeoutMs) {
  return new Promise((resolvePromise, reject) => {
    let settled = false;
    const finish = (callback, value) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      listener.off('error', onError);
      callback(value);
    };
    const onError = (error) => finish(reject, error);
    const timer = setTimeout(() => {
      try {
        listener.closeAllConnections();
      } catch (error) {
        finish(reject, error);
        return;
      }
      finish(
        reject,
        new Error(`package verifier listener close timed out after ${timeoutMs} ms`),
      );
    }, timeoutMs);

    listener.once('error', onError);
    try {
      listener.closeAllConnections();
      listener.close((error) => {
        if (error && error.code !== 'ERR_SERVER_NOT_RUNNING') {
          finish(reject, error);
        } else {
          finish(resolvePromise);
        }
      });
    } catch (error) {
      finish(reject, error);
    }
  });
}

export function postInitialize(
  port,
  {
    maxResponseBytes = initializeResponseMaxBytes,
    requestImplementation = httpRequest,
    timeoutMs = initializeTimeoutMs,
  } = {},
) {
  assertPositiveSafeInteger(maxResponseBytes, 'initialize response byte limit');
  const payload = JSON.stringify({
    jsonrpc: '2.0',
    id: 'package-verifier-initialize',
    method: 'initialize',
    params: {
      protocolVersion: initializeProtocolVersion,
      capabilities: {},
      clientInfo: initializeClientInfo,
    },
  });

  return new Promise((resolvePromise, reject) => {
    const controller = new AbortController();
    const chunks = [];
    let request;
    let response;
    let settled = false;
    let terminalError;
    let observedBytes = 0;
    let retainedBytes = 0;
    const rejectTerminal = (error) => {
      if (terminalError) {
        queueMicrotask(() => finish(reject, terminalError));
        return;
      }
      finish(reject, error);
    };
    const terminateWithPrimaryError = (error) => {
      if (settled || terminalError) {
        return;
      }
      terminalError = error;
      const cleanupErrors = [];
      response?.off('data', onResponseData);
      try {
        controller.abort(error);
      } catch (cleanupError) {
        cleanupErrors.push(
          normalizeThrownValue(cleanupError, 'initialize request abort cleanup'),
        );
      }
      try {
        response?.destroy();
      } catch (cleanupError) {
        cleanupErrors.push(
          normalizeThrownValue(cleanupError, 'initialize response destroy cleanup'),
        );
      }
      try {
        request?.destroy();
      } catch (cleanupError) {
        cleanupErrors.push(
          normalizeThrownValue(cleanupError, 'initialize request destroy cleanup'),
        );
      }
      if (cleanupErrors.length > 0) {
        terminalError = new AggregateError(
          [error, ...cleanupErrors],
          'initialize request failed and teardown also failed',
        );
      }
      queueMicrotask(() => finish(reject, terminalError));
    };
    const onRequestError = (error) => rejectTerminal(error);
    const onResponseData = (chunk) => {
      const chunkBytes = Buffer.byteLength(chunk);
      observedBytes += chunkBytes;
      if (observedBytes > maxResponseBytes) {
        const error = new Error(
          `initialize response exceeded maximum size: observed ${observedBytes} bytes, allowed ${maxResponseBytes} bytes`,
        );
        error.allowedBytes = maxResponseBytes;
        error.observedBytes = observedBytes;
        error.retainedBytes = retainedBytes;
        terminateWithPrimaryError(error);
        return;
      }
      chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
      retainedBytes += chunkBytes;
    };
    const onResponseError = (error) => rejectTerminal(error);
    const onResponseAborted = () => {
      rejectTerminal(
        new Error('initialize response was aborted before the complete body arrived'),
      );
    };
    const onResponseClose = () => {
      rejectTerminal(
        new Error('initialize response closed before the complete body arrived'),
      );
    };
    const onResponseEnd = () => {
      if (terminalError) {
        rejectTerminal(terminalError);
        return;
      }
      finish(resolvePromise, {
        body: Buffer.concat(chunks, retainedBytes).toString('utf8'),
        headers: response.headers,
        statusCode: response.statusCode,
      });
    };
    const removeListeners = () => {
      request?.off('error', onRequestError);
      response?.off('data', onResponseData);
      response?.off('error', onResponseError);
      response?.off('aborted', onResponseAborted);
      response?.off('close', onResponseClose);
      response?.off('end', onResponseEnd);
    };
    const finish = (callback, value) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      removeListeners();
      callback(value);
    };
    const timer = setTimeout(() => {
      terminateWithPrimaryError(
        new Error(
          `initialize request timed out after ${timeoutMs} ms before the complete response body was received`,
        ),
      );
    }, timeoutMs);

    try {
      request = requestImplementation(
        {
          host: '127.0.0.1',
          port,
          path: '/mcp',
          method: 'POST',
          headers: {
            accept: 'application/json, text/event-stream',
            'content-length': Buffer.byteLength(payload),
            'content-type': 'application/json',
          },
          signal: controller.signal,
        },
        (incomingResponse) => {
          if (settled) {
            incomingResponse.destroy();
            return;
          }
          response = incomingResponse;
          response.on('data', onResponseData);
          response.on('error', onResponseError);
          response.on('aborted', onResponseAborted);
          response.on('close', onResponseClose);
          response.on('end', onResponseEnd);
        },
      );
      request.on('error', onRequestError);
      request.end(payload);
    } catch (error) {
      finish(reject, error);
    }
  });
}

export async function runWithCleanup(
  operation,
  cleanupTasks,
  message = 'package verifier failed and could not clean up every resource',
) {
  const cleanupErrors = [];
  let primaryError;
  let primaryFailed = false;
  let result;

  try {
    result = await operation();
  } catch (error) {
    primaryFailed = true;
    primaryError = error;
  }

  for (const cleanup of cleanupTasks) {
    try {
      await cleanup();
    } catch (error) {
      cleanupErrors.push(error);
    }
  }

  throwCollectedErrors(primaryFailed, primaryError, cleanupErrors, message);
  return result;
}

export function createVerifierCleanupTasks(
  resources,
  {
    closeListenerImplementation = closeListener,
    removeWorkspace = rm,
    resourceTimeoutMs = cleanupTimeoutMs,
    workspaceTimeoutMs = workspaceCleanupTimeoutMs,
  } = {},
) {
  const closeResource = (key, label) => () =>
    resources[key] &&
    withDeadline(
      () => resources[key].close(),
      resourceTimeoutMs,
      `package verifier ${label} close`,
    );

  return [
    closeResource('stdioClient', 'stdio client'),
    closeResource('stdioTransport', 'stdio transport'),
    closeResource('httpClient', 'HTTP client'),
    closeResource('httpTransport', 'HTTP transport'),
    () =>
      resources.httpListener &&
      closeListenerImplementation(resources.httpListener, resourceTimeoutMs),
    closeResource('transport', 'transport'),
    closeResource('server', 'server'),
    () =>
      resources.listener &&
      closeListenerImplementation(resources.listener, resourceTimeoutMs),
    () =>
      resources.workspace &&
      withDeadline(
        () => removeWorkspace(resources.workspace, { recursive: true, force: true }),
        workspaceTimeoutMs,
        'package verifier workspace removal',
      ),
  ];
}

async function verifyPackage(resources) {
  let listener;
  let server;
  let transport;
  let workspace;

  const npmCliPath = await validatedNpmCliPath();
  resources.workspace = workspace = await mkdtemp(join(tmpdir(), 'happy-platform-mcp-package-'));
  const packDir = join(workspace, 'pack');
  const consumerDir = join(workspace, 'consumer');
  await Promise.all([
    mkdir(packDir),
    mkdir(consumerDir),
  ]);

  const sourceSdkManifest = await readFile(sdkManifestPath);
  const sourceSdk = JSON.parse(sourceSdkManifest.toString('utf8'));
  assert(sourceSdk.name === '@modelcontextprotocol/sdk' && sourceSdk.version === '1.32.1', 'source SDK is not @modelcontextprotocol/sdk@1.32.1');
  assert(sourceSdk.dependencies?.['@hono/node-server'] === '^1.19.9 || ^2.0.5', 'source SDK does not retain its native Hono range');
  const lock = JSON.parse(await readFile(join(root, 'package-lock.json'), 'utf8'));
  for (const [path, version] of Object.entries({
    'node_modules/@modelcontextprotocol/sdk': '1.32.1', 'node_modules/@hono/node-server': '2.1.3',
    'node_modules/hono': '4.13.8', 'node_modules/axios': '1.20.0', 'node_modules/fast-uri': '3.1.8',
    'node_modules/qs': '6.16.0', 'node_modules/brace-expansion': '5.0.12',
  })) {
    assert(lock.packages?.[path]?.version === version, 'committed lock has an unexpected version for ' + path);
  }
  const bundledPaths = Object.entries(lock.packages).filter(([, entry]) => entry.inBundle).map(([path]) => path);
  assert(bundledPaths.includes('node_modules/@modelcontextprotocol/sdk'), 'committed lock is missing the SDK bundle');
  const pack = parsePackOutput(runNpm(npmCliPath, ['pack', '--json', '--pack-destination', packDir]));
  assert((await readFile(sdkManifestPath)).equals(sourceSdkManifest), 'npm pack mutated the installed SDK manifest');
  const tarballPath = join(packDir, pack.filename);
  assertPackageFiles(pack.files, bundledPaths);
  const packedPaths = new Set(pack.files.map(({ path }) => path));
  for (const requiredPath of ['package.json', 'scripts/verify-package.mjs', 'src/cli.js', 'src/server.js',
    'src/config/comprehensive-table-definitions.json', 'node_modules/@modelcontextprotocol/sdk/package.json',
    'node_modules/@hono/node-server/package.json']) {
    assert(packedPaths.has(requiredPath), 'packed tarball is missing ' + requiredPath);
  }

  const consumerManifest = {
    name: 'happy-platform-mcp-package-consumer',
    version: '1.0.0',
    private: true,
    type: 'module',
    dependencies: {
      'happy-platform-mcp': `file:${tarballPath}`,
    },
  };
  assert(!Object.hasOwn(consumerManifest, 'overrides'), 'consumer manifest unexpectedly contains overrides');
  await writeFile(join(consumerDir, 'package.json'), `${JSON.stringify(consumerManifest, null, 2)}\n`);

  runNpm(
    npmCliPath,
    ['install', '--ignore-scripts=false', '--audit=false', '--fund=false'],
    consumerDir,
  );

  const consumerRequire = createRequire(join(consumerDir, 'package.json'));
  const installedPackageManifest = consumerRequire.resolve('happy-platform-mcp/package.json');
  const installedPackageRoot = dirname(installedPackageManifest);
  const packedRoot = JSON.parse(await readFile(installedPackageManifest, 'utf8'));
  const sourceRoot = JSON.parse(await readFile(join(root, 'package.json'), 'utf8'));
  const packedSdk = JSON.parse(
    await readFile(
      join(installedPackageRoot, 'node_modules', '@modelcontextprotocol', 'sdk', 'package.json'),
      'utf8',
    ),
  );
  const packedHono = JSON.parse(
    await readFile(
      join(installedPackageRoot, 'node_modules', '@hono', 'node-server', 'package.json'),
      'utf8',
    ),
  );
  assert(
    packedRoot.main === sourceRoot.main,
    `installed main target ${inspect(packedRoot.main)} does not match package.json ${inspect(sourceRoot.main)}`,
  );
  assert(
    JSON.stringify(packedRoot.bin) === JSON.stringify(sourceRoot.bin),
    `installed bin targets ${inspect(packedRoot.bin)} do not exactly match package.json ${inspect(sourceRoot.bin)}`,
  );
  const installedEntrypoints = [packedRoot.main, ...Object.values(packedRoot.bin ?? {})];
  for (const target of installedEntrypoints) {
    assert(typeof target === 'string' && target.length > 0, 'installed package has an empty entrypoint target');
    const metadata = await lstat(join(installedPackageRoot, target));
    assert(metadata.isFile(), `installed entrypoint target is not a file: ${target}`);
  }

  assert(packedRoot.dependencies?.['@modelcontextprotocol/sdk'] === '1.32.1', 'packed root SDK dependency is not 1.32.1');
  assert(packedRoot.dependencies?.axios === '1.20.0', 'packed root Axios dependency is not 1.20.0');
  assert(packedRoot.engines?.node === '>=20', 'packed root Node engine is not >=20');
  assert(packedRoot.bundleDependencies?.length === 1 && packedRoot.bundleDependencies[0] === '@modelcontextprotocol/sdk', 'packed root does not retain the SDK-only bundle');
  assert(!['prepack', 'postpack', 'postinstall'].some((name) => Object.hasOwn(packedRoot.scripts ?? {}, name)), 'packed root contains an obsolete mutation lifecycle');
  assert(packedRoot.scripts?.['test:package'] === 'node scripts/verify-package.mjs', 'packed root verifier script is missing');
  assert(!Object.hasOwn(packedRoot.overrides ?? {}, '@modelcontextprotocol/sdk'), 'packed root retains an obsolete SDK override');
  assert(packedSdk.name === '@modelcontextprotocol/sdk' && packedSdk.version === '1.32.1', 'packed SDK is not @modelcontextprotocol/sdk@1.32.1');
  assert(packedSdk.dependencies?.['@hono/node-server'] === '^1.19.9 || ^2.0.5', 'packed SDK does not retain its native Hono range');
  assert((await readFile(join(installedPackageRoot, 'node_modules', '@modelcontextprotocol', 'sdk', 'package.json'))).equals(sourceSdkManifest), 'packaged SDK manifest differs from its unmodified installed source');
  assert(packedHono.name === '@hono/node-server' && packedHono.version === '2.1.3', 'packed Hono is not @hono/node-server@2.1.3');
  for (const directory of bundledPaths) {
    const relativeManifest = directory + '/package.json';
    assert(packedPaths.has(relativeManifest), 'packed tarball is missing bundled manifest ' + directory);
    const manifestBytes = await readFile(join(installedPackageRoot, relativeManifest));
    assert(JSON.parse(manifestBytes).version === lock.packages[directory].version, 'installed bundle differs from committed lock: ' + directory);
    assert(manifestBytes.equals(await readFile(join(root, relativeManifest))), 'installed bundle manifest differs from packed source: ' + directory);
  }

  const treeOutput = runNpm(
    npmCliPath,
    ['ls', '@modelcontextprotocol/sdk', '@hono/node-server', 'axios', '--all'],
    consumerDir,
  );
  const tree = JSON.parse(
    runNpm(
      npmCliPath,
      ['ls', '@modelcontextprotocol/sdk', '@hono/node-server', 'axios', '--all', '--json'],
      consumerDir,
    ),
  );
  requireOnlyVersion(tree, '@modelcontextprotocol/sdk', '1.32.1');
  requireOnlyVersion(tree, '@hono/node-server', '2.1.3');
  requireOnlyVersion(tree, 'axios', '1.20.0');

  const auditOutput = runNpm(npmCliPath, ['audit', '--audit-level=low'], consumerDir);

  const packageRequire = createRequire(installedPackageManifest);
  const [
    clientModule,
    sseClientTransportModule,
    stdioClientTransportModule,
    installedHttpModule,
  ] = await Promise.all([
    import(pathToFileURL(packageRequire.resolve('@modelcontextprotocol/sdk/client/index.js')).href),
    import(pathToFileURL(packageRequire.resolve('@modelcontextprotocol/sdk/client/sse.js')).href),
    import(pathToFileURL(packageRequire.resolve('@modelcontextprotocol/sdk/client/stdio.js')).href),
    import(pathToFileURL(join(installedPackageRoot, 'src', 'http-server.js')).href),
  ]);
  assert(typeof clientModule.Client === 'function', 'installed SDK Client import is not callable');
  assert(
    typeof stdioClientTransportModule.StdioClientTransport === 'function',
    'installed StdioClientTransport import is not callable',
  );
  assert(
    typeof sseClientTransportModule.SSEClientTransport === 'function',
    'installed SSEClientTransport import is not callable',
  );
  assert(
    typeof installedHttpModule.createHttpApp === 'function',
    'installed src/http-server.js createHttpApp import is not callable',
  );

  const stdioBinTarget = packedRoot.bin['happy-platform-mcp'];
  resources.stdioTransport = new stdioClientTransportModule.StdioClientTransport({
    command: process.execPath,
    args: [join(installedPackageRoot, stdioBinTarget), '--docs-only'],
    cwd: consumerDir,
    env: {
      ...process.env,
      HAPPY_CONFIG_PATH: join(consumerDir, '__missing-package-verifier-config.json'),
      HAPPY_DOCS_ENABLE_LOCAL_INDEX: 'false',
      HAPPY_DOCS_ENABLE_VECTOR: 'false',
      HAPPY_MCP_DOCS_ONLY: 'true',
      SERVICENOW_INSTANCE_URL: '',
      SERVICENOW_PASSWORD: '',
      SERVICENOW_USERNAME: '',
    },
    stderr: 'inherit',
  });
  resources.stdioClient = new clientModule.Client({
    name: 'installed-package-stdio-verifier',
    version: '1.0.0',
  });
  await withDeadline(
    () => resources.stdioClient.connect(resources.stdioTransport),
    initializeTimeoutMs,
    'installed stdio client connect',
  );
  const stdioTools = await withDeadline(
    () => resources.stdioClient.listTools(),
    initializeTimeoutMs,
    'installed stdio listTools',
  );
  const stdioResult = await withDeadline(
    () => resources.stdioClient.callTool({ name: 'SN-Docs-Status', arguments: {} }),
    initializeTimeoutMs,
    'installed stdio SN-Docs-Status',
  );
  const stdioStatus = JSON.parse(stdioResult.content?.[0]?.text);
  assert(
    JSON.stringify(resources.stdioClient.getServerVersion()) ===
      JSON.stringify({ name: 'servicenow-server', version: '2.0.0' }),
    'installed stdio server identity is unexpected',
  );
  assert(
    stdioTools.tools.some((tool) => tool.name === 'SN-Docs-Status'),
    'installed stdio listTools is missing SN-Docs-Status',
  );
  assert(stdioResult.isError !== true, 'installed stdio SN-Docs-Status returned an MCP error');
  assert(
    stdioStatus.localIndexEnabled === false &&
      stdioStatus.ftsAvailable === false &&
      Array.isArray(stdioStatus.families) &&
      stdioStatus.families.length === 0,
    `installed stdio SN-Docs-Status returned unexpected status: ${inspect(stdioStatus)}`,
  );

  const fakeRecords = [{ sys_id: 'package-verifier-record', short_description: 'Installed HTTP smoke' }];
  const fakeCalls = [];
  const fakeServiceNowClient = {
    setProgressCallback() {},
    async getRecords(table, params) {
      fakeCalls.push({ params, table });
      return fakeRecords;
    },
  };
  const installedHttpToken = randomBytes(32).toString('hex');
  const installedHttpApp = installedHttpModule.createHttpApp({
    apiToken: installedHttpToken,
    defaultInstance: {
      name: 'package-verifier',
      url: 'https://package-verifier.invalid',
    },
    createServiceNowClient: () => fakeServiceNowClient,
  });
  resources.httpListener = createServer(installedHttpApp);
  const installedHttpPort = await listenOnLoopback(resources.httpListener);
  const installedHealthUrl = `http://127.0.0.1:${installedHttpPort}/health`;
  const unauthenticatedHealth = await withDeadline(
    () => fetch(installedHealthUrl),
    initializeTimeoutMs,
    'installed HTTP unauthenticated health',
  );
  await unauthenticatedHealth.body?.cancel();
  assert(
    unauthenticatedHealth.status === 401,
    `installed HTTP /health without a bearer returned ${unauthenticatedHealth.status}, expected 401`,
  );
  const authenticatedHealth = await withDeadline(
    () => fetch(installedHealthUrl, { headers: { authorization: `Bearer ${installedHttpToken}` } }),
    initializeTimeoutMs,
    'installed HTTP authenticated health',
  );
  await authenticatedHealth.body?.cancel();
  assert(
    authenticatedHealth.status === 200,
    `installed HTTP /health with the bearer returned ${authenticatedHealth.status}, expected 200`,
  );
  resources.httpTransport = new sseClientTransportModule.SSEClientTransport(
    new URL(`http://127.0.0.1:${installedHttpPort}/mcp`),
    { requestInit: { headers: { authorization: `Bearer ${installedHttpToken}` } } },
  );
  resources.httpClient = new clientModule.Client({
    name: 'installed-package-http-verifier',
    version: '1.0.0',
  });
  await withDeadline(
    () => resources.httpClient.connect(resources.httpTransport),
    initializeTimeoutMs,
    'installed HTTP client connect',
  );
  const httpTools = await withDeadline(
    () => resources.httpClient.listTools(),
    initializeTimeoutMs,
    'installed HTTP listTools',
  );
  const httpResult = await withDeadline(
    () =>
      resources.httpClient.callTool({
        name: 'SN-Query-Table',
        arguments: {
          fields: 'sys_id,short_description',
          limit: 1,
          query: 'active=true',
          table_name: 'incident',
        },
      }),
    initializeTimeoutMs,
    'installed HTTP SN-Query-Table',
  );
  assert(
    JSON.stringify(resources.httpClient.getServerVersion()) ===
      JSON.stringify({ name: 'servicenow-server', version: '2.0.0' }),
    'installed HTTP server identity is unexpected',
  );
  assert(
    httpTools.tools.some((tool) => tool.name === 'SN-Query-Table'),
    'installed HTTP listTools is missing SN-Query-Table',
  );
  assert(httpResult.isError !== true, 'installed HTTP SN-Query-Table returned an MCP error');
  assert(
    fakeCalls.length === 1 &&
      fakeCalls[0].table === 'incident' &&
      fakeCalls[0].params.sysparm_limit === 1 &&
      fakeCalls[0].params.sysparm_query === 'active=true' &&
      fakeCalls[0].params.sysparm_fields === 'sys_id,short_description' &&
      fakeCalls[0].params.sysparm_offset === undefined &&
      JSON.stringify(Object.keys(fakeCalls[0].params).sort()) ===
        JSON.stringify(['sysparm_fields', 'sysparm_limit', 'sysparm_offset', 'sysparm_query']),
    `installed HTTP fake client received unexpected calls: ${inspect(fakeCalls)}`,
  );
  assert(
    JSON.stringify(httpResult.content) ===
      JSON.stringify([
        {
          type: 'text',
          text: `Found 1 records in incident:\n${JSON.stringify(fakeRecords, null, 2)}`,
        },
      ]),
    `installed HTTP SN-Query-Table returned unexpected result: ${inspect(httpResult)}`,
  );
  const serverModule = await import(
    pathToFileURL(packageRequire.resolve('@modelcontextprotocol/sdk/server/index.js')).href
  );
  const transportModule = await import(
    pathToFileURL(packageRequire.resolve('@modelcontextprotocol/sdk/server/streamableHttp.js')).href
  );
  assert(typeof serverModule.Server === 'function', 'production SDK Server import is not callable');
  assert(
    typeof transportModule.StreamableHTTPServerTransport === 'function',
    'production StreamableHTTPServerTransport import is not callable',
  );

  resources.server = server = new serverModule.Server(initializeServerInfo, {
    capabilities: {},
  });
  resources.transport = transport = new transportModule.StreamableHTTPServerTransport({
    enableJsonResponse: true,
    sessionIdGenerator: () => initializeSessionId,
  });
  resources.listener = listener = createServer((request, response) => {
    transport.handleRequest(request, response).catch((error) => {
      if (!response.headersSent) {
        response.writeHead(500, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ error: error.message }));
      } else {
        response.destroy(error);
      }
    });
  });

  let initializeResponse;
  await server.connect(transport);
  const port = await listenOnLoopback(listener);
  initializeResponse = await postInitialize(port);

  assert(initializeResponse.statusCode === 200, `initialize returned HTTP ${initializeResponse.statusCode}`);
  assert(
    initializeResponse.headers['mcp-session-id'] === initializeSessionId,
    'initialize response has an unexpected MCP session header',
  );
  const contentType = initializeResponse.headers['content-type'];
  assert(
    typeof contentType === 'string' && /^application\/json(?:;|$)/i.test(contentType),
    `initialize response is not JSON: ${contentType ?? 'missing content-type'}`,
  );

  let initializeBody;
  try {
    initializeBody = JSON.parse(initializeResponse.body);
  } catch (error) {
    throw new Error(`initialize response body is not valid JSON: ${initializeResponse.body}`, {
      cause: error,
    });
  }
  assert(!Array.isArray(initializeBody), 'initialize response unexpectedly returned a JSON-RPC batch');
  assert(initializeBody.jsonrpc === '2.0', 'initialize response has an unexpected JSON-RPC version');
  assert(
    initializeBody.id === 'package-verifier-initialize',
    'initialize response has an unexpected JSON-RPC id',
  );
  assert(
    initializeBody.result?.protocolVersion === initializeProtocolVersion,
    'initialize response negotiated an unexpected MCP protocol version',
  );
  assert(
    initializeBody.result?.serverInfo?.name === initializeServerInfo.name &&
      initializeBody.result?.serverInfo?.version === initializeServerInfo.version,
    'initialize response has unexpected server identity semantics',
  );
  assert(
    initializeBody.result?.capabilities &&
      typeof initializeBody.result.capabilities === 'object',
    'initialize response is missing server capabilities',
  );
  assert(
    JSON.stringify(server.getClientVersion()) === JSON.stringify(initializeClientInfo),
    'server did not retain initialize client identity semantics',
  );
  assert(
    JSON.stringify(server.getClientCapabilities()) === '{}',
    'server did not retain initialize client capability semantics',
  );

  return {
    auditOutput,
    entrypoints: {
      bins: packedRoot.bin,
      main: packedRoot.main,
    },
    fileCount: pack.entryCount ?? pack.files.length,
    filename: pack.filename,
    http: {
      resultText: httpResult.content[0].text,
      serverName: resources.httpClient.getServerVersion().name,
      toolCount: httpTools.tools.length,
      unauthenticatedStatus: unauthenticatedHealth.status,
    },
    initialize: {
      clientName: server.getClientVersion().name,
      contentType,
      protocolVersion: initializeBody.result.protocolVersion,
      serverName: initializeBody.result.serverInfo.name,
      sessionId: initializeResponse.headers['mcp-session-id'],
      statusCode: initializeResponse.statusCode,
    },
    stdio: {
      serverName: resources.stdioClient.getServerVersion().name,
      toolCount: stdioTools.tools.length,
    },
    sdkManifestSha256: sha256(sourceSdkManifest),
    size: pack.size,
    treeOutput,
    unpackedSize: pack.unpackedSize,
  };
}

async function main() {
  const resources = {};
  const summary = await runWithCleanup(
    () => verifyPackage(resources),
    createVerifierCleanupTasks(resources),
  );

  console.log(`PASS package tarball ${summary.filename}: ${summary.size} bytes compressed, ${summary.unpackedSize} bytes unpacked, ${summary.fileCount} files`);
  console.log('PASS SDK manifest bundled without mutation: sha256 ' + summary.sdkManifestSha256);
  console.log('PASS tarball runtime file allow-list and secret/cache/backup/temp exclusions');
  console.log(
    `PASS installed entrypoints main ${summary.entrypoints.main}, bins ${Object.entries(summary.entrypoints.bins).map(([name, target]) => `${name}=${target}`).join(', ')}`,
  );
  console.log(
    `PASS installed stdio ${summary.stdio.serverName}, ${summary.stdio.toolCount} tools, SN-Docs-Status`,
  );
  console.log(
    `PASS installed HTTP ${summary.http.serverName}, unauthenticated /health HTTP ${summary.http.unauthenticatedStatus}, bearer-authenticated ${summary.http.toolCount} tools, SN-Query-Table fake result ${JSON.stringify(summary.http.resultText)}`,
  );
  console.log(summary.treeOutput);
  console.log(summary.auditOutput);
  console.log(
    `PASS direct Hono initialize HTTP ${summary.initialize.statusCode}, ${summary.initialize.contentType}, session ${summary.initialize.sessionId}, protocol ${summary.initialize.protocolVersion}, server ${summary.initialize.serverName}, client ${summary.initialize.clientName}`,
  );
  console.log('PASS temporary package and consumer directories cleaned; no repository tarball created');
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  await main();
}
