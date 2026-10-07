import fs from 'fs/promises';
import path from 'path';
import { chunkMarkdown } from './chunker.js';
import { assertDocsBranchName, assertDocsFamilyName, normalizeDocsDocumentPath, resolveDocsCachePath } from './config.js';
import {
  DOCS_LIMITS,
  DOCS_RAW_ORIGIN,
  DOCS_REPO_RAW_BASE,
  DocsError,
  scanMarkdownLinks
} from './github-client.js';
import { createDocsStore } from './sqlite-store.js';

export const MAX_REPORTED_SKIPS = 50;
const MAX_REPORTED_TARGET_LENGTH = 200;
const SCHEME_PATTERN = /^[A-Za-z][A-Za-z0-9+.-]*:/;
const FATAL_SYNC_ERRORS = new Set(['DOCS_SYNC_BUDGET', 'DOCS_SYNC_DEADLINE', 'DOCS_ABORTED']);

function isMarkdownTarget(target) {
  let end = target.length;
  for (const marker of ['?', '#']) {
    const index = target.indexOf(marker);
    if (index !== -1 && index < end) end = index;
  }
  return target.slice(0, end).endsWith('.md');
}

/**
 * Maps a family llms.txt link to a document path inside the approved branch.
 * Only relative paths and raw links to the pinned repository and the same
 * branch are accepted; everything else is rejected before any URL is built.
 */
export function resolveDocsLink(target, branch) {
  let candidate = target;
  if (target.startsWith(DOCS_REPO_RAW_BASE)) {
    const rest = target.slice(DOCS_REPO_RAW_BASE.length);
    const slash = rest.indexOf('/');
    if (slash === -1) return { reason: 'invalid-path' };
    if (rest.slice(0, slash) !== branch) return { reason: 'cross-branch' };
    candidate = rest.slice(slash + 1);
  } else if (target.startsWith(`${DOCS_RAW_ORIGIN}/`) || target.startsWith('/')) {
    return { reason: 'cross-repository' };
  } else if (SCHEME_PATTERN.test(target) || target.startsWith('//')) {
    return { reason: 'unsupported-origin' };
  }

  try {
    return { path: normalizeDocsDocumentPath(candidate) };
  } catch {
    return { reason: 'invalid-path' };
  }
}

function reportedTarget(target) {
  return target.length > MAX_REPORTED_TARGET_LENGTH
    ? `${target.slice(0, MAX_REPORTED_TARGET_LENGTH)}…`
    : target;
}

export function parseMarkdownLinks(llmsText, { branch, maxLinks = DOCS_LIMITS.maxLinks }) {
  const links = [];
  const seen = new Set();
  const rejected = [];
  let rejectedCount = 0;

  scanMarkdownLinks(llmsText, (_label, target) => {
    if (!isMarkdownTarget(target)) return;
    const resolved = resolveDocsLink(target, branch);
    if (!resolved.path) {
      rejectedCount += 1;
      if (rejected.length < MAX_REPORTED_SKIPS) {
        rejected.push({ target: reportedTarget(target), reason: resolved.reason });
      }
      return;
    }
    if (seen.has(resolved.path)) return;
    if (links.length >= maxLinks) {
      throw new DocsError(
        'DOCS_TOO_MANY_LINKS',
        `ServiceNow docs family index lists more than ${maxLinks} documents; refusing to sync a truncated set`
      );
    }
    seen.add(resolved.path);
    links.push(resolved.path);
  });

  return { links, rejected, rejectedCount };
}

function skipReason(error) {
  return error instanceof DocsError ? error.message : 'ServiceNow docs request failed';
}

// Filesystem errors carry absolute local paths; report only the docs-relative
// label and the errno code.
async function cacheIo(label, operation) {
  try {
    return await operation();
  } catch (error) {
    const code = typeof error?.code === 'string' && /^[A-Z0-9_]{1,32}$/.test(error.code) ? error.code : 'EIO';
    throw new DocsError('DOCS_CACHE_WRITE', `Failed to update the ServiceNow docs cache for ${label} (${code})`);
  }
}

export async function syncDocsFamily({
  family,
  branch,
  cacheDir,
  client,
  vectorConfig = null,
  limits = DOCS_LIMITS,
  storeFactory = createDocsStore
}) {
  assertDocsFamilyName(family);
  if (branch !== undefined && branch !== null) assertDocsBranchName(branch);

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(new DocsError(
    'DOCS_SYNC_DEADLINE',
    `ServiceNow docs sync exceeded its ${limits.syncTimeoutMs} ms deadline`
  )), limits.syncTimeoutMs);
  const { signal } = controller;

  try {
    const approved = await client.resolveFamily(family, { signal });
    if (branch !== undefined && branch !== null && branch !== approved.branch) {
      throw new DocsError(
        'DOCS_BRANCH_MISMATCH',
        `Docs branch override "${branch}" does not match the approved ref "${approved.branch}" for family "${family}"`
      );
    }

    const budget = { remaining: limits.syncBytes };
    const llms = await client.getLlms(family, { signal, budget });
    const { links, rejected, rejectedCount } = parseMarkdownLinks(llms, {
      branch: approved.branch,
      maxLinks: limits.maxLinks
    });

    await cacheIo('the cache directory', () => fs.mkdir(cacheDir, { recursive: true }));
    const store = await storeFactory(path.join(cacheDir, 'index.sqlite'), { vectorConfig });
    try {
      store.initialize();
      const { invalidated } = store.beginFamilySync({ name: family, branch: approved.branch });
      if (invalidated) {
        await cacheIo(family, () => fs.rm(resolveDocsCachePath(cacheDir, family), { recursive: true, force: true }));
      }

      let documentsSynced = 0;
      let documentsSkipped = 0;
      const skippedDocuments = [];
      for (const link of links) {
        if (signal.aborted) throw signal.reason;
        let markdown;
        try {
          markdown = await client.getMarkdown(family, link, { signal, budget });
        } catch (error) {
          if (FATAL_SYNC_ERRORS.has(error?.code)) throw error;
          documentsSkipped += 1;
          if (skippedDocuments.length < MAX_REPORTED_SKIPS) {
            skippedDocuments.push({ path: link, error: skipReason(error) });
          }
          continue;
        }

        const outputPath = resolveDocsCachePath(cacheDir, family, link);
        await cacheIo(`${family}/${link}`, async () => {
          await fs.mkdir(path.dirname(outputPath), { recursive: true });
          await fs.writeFile(outputPath, markdown, 'utf8');
        });

        const chunks = chunkMarkdown({ family, path: link, markdown });
        store.replaceDocument({
          family,
          branch: approved.branch,
          path: link,
          sha: null,
          title: chunks[0]?.title || link,
          markdown
        }, chunks);
        documentsSynced += 1;
      }

      if (documentsSynced === 0 && documentsSkipped > 0) {
        throw new Error(`ServiceNow docs sync failed: all ${documentsSkipped} documents were skipped`);
      }

      const removedPaths = store.completeFamilySync({
        name: family,
        branch: approved.branch,
        syncedAt: new Date().toISOString(),
        keepPaths: links
      });
      for (const removedPath of removedPaths) {
        await cacheIo(`${family}/${removedPath}`, () => fs.rm(resolveDocsCachePath(cacheDir, family, removedPath), { force: true }));
      }

      return {
        family,
        branch: approved.branch,
        documentsSynced,
        documentsSkipped,
        skippedDocuments,
        ...(documentsSkipped > skippedDocuments.length ? { skippedDocumentsTruncated: true } : {}),
        documentsRemoved: removedPaths.length,
        linksRejected: rejectedCount,
        rejectedLinks: rejected,
        ...(rejectedCount > rejected.length ? { rejectedLinksTruncated: true } : {})
      };
    } finally {
      store.close();
    }
  } finally {
    clearTimeout(timer);
  }
}
