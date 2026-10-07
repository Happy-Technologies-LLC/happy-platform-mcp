import { assertDocsFamilyName, normalizeDocsDocumentPath } from './config.js';

// Production origin pin: every request goes to this exact repository on the
// raw GitHub origin. There is intentionally no base-URL override.
export const DOCS_RAW_ORIGIN = 'https://raw.githubusercontent.com';
export const DOCS_REPO_RAW_BASE = `${DOCS_RAW_ORIGIN}/ServiceNow/ServiceNowDocs/`;
export const DOCS_INDEX_REF = 'main';

const MiB = 1024 * 1024;
export const DOCS_LIMITS = Object.freeze({
  indexBytes: MiB,
  documentBytes: 8 * MiB,
  lineBytes: 16 * 1024,
  maxLinks: 1000,
  maxFamilies: 100,
  syncBytes: 64 * MiB,
  requestTimeoutMs: 30_000,
  syncTimeoutMs: 600_000
});

export class DocsError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'DocsError';
    this.code = code;
  }
}

function isSafeName(value) {
  try {
    assertDocsFamilyName(value);
    return true;
  } catch {
    return false;
  }
}

/**
 * Single forward pass over `[text](target)` links. Each line is scanned once:
 * the backward search for "[" never crosses the previous link's end, so the
 * total work is linear in the input size.
 */
export function scanMarkdownLinks(text, visit) {
  let lineStart = 0;
  for (;;) {
    const newline = text.indexOf('\n', lineStart);
    const line = text.slice(lineStart, newline === -1 ? text.length : newline);
    let pos = 0;
    while (pos < line.length) {
      const mid = line.indexOf('](', pos);
      if (mid === -1) break;
      const close = line.indexOf(')', mid + 2);
      if (close === -1) break;
      let open = mid - 1;
      while (open >= pos && line[open] !== '[') open -= 1;
      if (open >= pos) {
        visit(line.slice(open + 1, mid).trim(), line.slice(mid + 2, close).trim());
      }
      pos = close + 1;
    }
    if (newline === -1) return;
    lineStart = newline + 1;
  }
}

// Upstream mapping line: `- "australia" : "australia" -- Australia family`
function parseMappingLine(line) {
  let rest = line.trimStart();
  if (rest.startsWith('- ') || rest.startsWith('* ')) rest = rest.slice(2).trimStart();
  if (!rest.startsWith('"')) return null;
  const nameEnd = rest.indexOf('"', 1);
  if (nameEnd === -1) return null;
  const name = rest.slice(1, nameEnd);
  rest = rest.slice(nameEnd + 1).trimStart();
  if (!rest.startsWith(':')) return null;
  rest = rest.slice(1).trimStart();
  if (!rest.startsWith('"')) return null;
  const branchEnd = rest.indexOf('"', 1);
  if (branchEnd === -1) return null;
  return { name, branch: rest.slice(1, branchEnd) };
}

/**
 * Approved families come only from the pinned main/llms.txt index, either as
 * the upstream family-to-branch mapping list or as links to a family's
 * llms.txt in the pinned repository. Unsafe or off-repository entries are
 * ignored; the first entry wins for a duplicate family or branch.
 */
export function parseFamiliesFromLlms(text, { maxFamilies = DOCS_LIMITS.maxFamilies } = {}) {
  const families = [];
  const names = new Set();
  const branches = new Set();

  function add(name, branch) {
    if (!isSafeName(name) || !isSafeName(branch) || names.has(name) || branches.has(branch)) return;
    if (families.length >= maxFamilies) {
      throw new DocsError('DOCS_INDEX_INVALID', `ServiceNow docs index lists more than ${maxFamilies} families`);
    }
    names.add(name);
    branches.add(branch);
    families.push({ name, branch });
  }

  for (const line of text.split('\n')) {
    const mapping = parseMappingLine(line);
    if (mapping) add(mapping.name, mapping.branch);
  }

  scanMarkdownLinks(text, (label, target) => {
    if (!target.startsWith(DOCS_REPO_RAW_BASE) || !target.endsWith('/llms.txt')) return;
    const branch = target.slice(DOCS_REPO_RAW_BASE.length, -'/llms.txt'.length);
    add(isSafeName(label) ? label : branch, branch);
  });

  return families;
}

function isRedirectFailure(error) {
  const texts = [error?.message, error?.cause?.message, error?.cause?.code];
  return texts.some((text) => typeof text === 'string' && /redirect/i.test(text));
}

async function readBoundedText(response, { maxBytes, lineBytes, budget, label }) {
  const tooLarge = () => new DocsError('DOCS_TOO_LARGE', `ServiceNow docs response for ${label} exceeds ${maxBytes} bytes`);
  const overBudget = () => new DocsError(
    'DOCS_SYNC_BUDGET',
    `ServiceNow docs sync exceeded its aggregate download budget while reading ${label}`
  );
  const declared = response.headers?.get?.('content-length');
  if (declared && /^\d+$/.test(declared)) {
    const length = Number(declared);
    if (length > maxBytes) throw tooLarge();
    if (budget && length > budget.remaining) throw overBudget();
  }
  if (!response.body) return '';

  const reader = response.body.getReader();
  const chunks = [];
  let total = 0;
  let currentLine = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      total += value.byteLength;
      if (total > maxBytes) throw tooLarge();
      if (budget) {
        budget.remaining -= value.byteLength;
        if (budget.remaining < 0) throw overBudget();
      }
      let start = 0;
      for (;;) {
        const newline = value.indexOf(10, start);
        currentLine += (newline === -1 ? value.byteLength : newline) - start;
        if (currentLine > lineBytes) {
          throw new DocsError('DOCS_LINE_TOO_LONG', `ServiceNow docs response for ${label} has a line longer than ${lineBytes} bytes`);
        }
        if (newline === -1) break;
        currentLine = 0;
        start = newline + 1;
      }
      chunks.push(value);
    }
  } catch (error) {
    reader.cancel().catch(() => {});
    throw error;
  }
  return Buffer.concat(chunks, total).toString('utf8');
}

async function fetchBoundedText(fetchImpl, url, { label, signal, timeoutMs, maxBytes, lineBytes, budget }) {
  const controller = new AbortController();
  let cause = null;
  const abort = (reason) => {
    cause ??= reason;
    controller.abort(cause);
  };
  const timer = setTimeout(
    () => abort(new DocsError('DOCS_TIMEOUT', `ServiceNow docs request for ${label} timed out after ${timeoutMs} ms`)),
    timeoutMs
  );
  const onOuterAbort = () => abort(signal.reason instanceof DocsError
    ? signal.reason
    : new DocsError('DOCS_ABORTED', `ServiceNow docs request for ${label} was aborted`));
  if (signal?.aborted) onOuterAbort();
  signal?.addEventListener('abort', onOuterAbort, { once: true });

  try {
    if (cause) throw cause;
    let response;
    try {
      response = await fetchImpl(url, {
        method: 'GET',
        headers: { Accept: 'text/plain' },
        redirect: 'error',
        signal: controller.signal
      });
    } catch (error) {
      if (cause) throw cause;
      if (isRedirectFailure(error)) {
        throw new DocsError('DOCS_REDIRECT', `ServiceNow docs request for ${label} was redirected; redirects are rejected`);
      }
      throw new DocsError('DOCS_NETWORK', `ServiceNow docs request for ${label} failed: network error`);
    }

    const status = Number(response.status);
    if (response.redirected || response.type === 'opaqueredirect' || (status >= 300 && status < 400)) {
      response.body?.cancel?.().catch(() => {});
      throw new DocsError('DOCS_REDIRECT', `ServiceNow docs request for ${label} was redirected; redirects are rejected`);
    }
    if (!response.ok) {
      response.body?.cancel?.().catch(() => {});
      const shownStatus = Number.isInteger(status) ? status : 'error';
      throw new DocsError('DOCS_HTTP_STATUS', `ServiceNow docs request for ${label} failed with HTTP ${shownStatus}`);
    }

    try {
      return await readBoundedText(response, { maxBytes, lineBytes, budget, label });
    } catch (error) {
      if (cause) throw cause;
      if (error instanceof DocsError) throw error;
      throw new DocsError('DOCS_NETWORK', `ServiceNow docs request for ${label} failed while reading the response`);
    }
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener('abort', onOuterAbort);
  }
}

export function createServiceNowDocsClient({
  fetchImpl = globalThis.fetch,
  limits = DOCS_LIMITS
} = {}) {
  if (!fetchImpl) {
    throw new Error('Fetch API is unavailable in this Node runtime');
  }

  let familiesPromise = null;

  function request(branch, relativePath, { signal, maxBytes, budget }) {
    const encodedPath = relativePath.split('/').map(encodeURIComponent).join('/');
    const url = `${DOCS_REPO_RAW_BASE}${encodeURIComponent(branch)}/${encodedPath}`;
    return fetchBoundedText(fetchImpl, url, {
      label: `${branch}/${relativePath}`,
      signal,
      budget,
      maxBytes,
      lineBytes: limits.lineBytes,
      timeoutMs: limits.requestTimeoutMs
    });
  }

  function listFamilies({ signal } = {}) {
    if (!familiesPromise) {
      familiesPromise = request(DOCS_INDEX_REF, 'llms.txt', { signal, maxBytes: limits.indexBytes })
        .then((text) => parseFamiliesFromLlms(text, limits));
      familiesPromise.catch(() => {
        familiesPromise = null;
      });
    }
    return familiesPromise.then((families) => families.map((family) => ({ ...family })));
  }

  async function resolveFamily(family, { signal } = {}) {
    assertDocsFamilyName(family);
    const approved = (await listFamilies({ signal })).find((entry) => entry.name === family);
    if (!approved) {
      throw new DocsError(
        'DOCS_UNKNOWN_FAMILY',
        `Unknown ServiceNow docs family "${family}"; use SN-Docs-Families to list families approved by the ServiceNowDocs index`
      );
    }
    return approved;
  }

  return {
    listFamilies,
    resolveFamily,

    async getLlms(family, { signal, budget } = {}) {
      const { branch } = await resolveFamily(family, { signal });
      return request(branch, 'llms.txt', { signal, budget, maxBytes: limits.indexBytes });
    },

    async getMarkdown(family, documentPath, { signal, budget } = {}) {
      const safePath = normalizeDocsDocumentPath(documentPath);
      const { branch } = await resolveFamily(family, { signal });
      return request(branch, safePath, { signal, budget, maxBytes: limits.documentBytes });
    }
  };
}
