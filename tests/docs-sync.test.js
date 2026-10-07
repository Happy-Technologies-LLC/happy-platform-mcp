import fs from 'fs/promises';
import os from 'os';
import path from 'path';
import { afterEach, describe, expect, test } from '@jest/globals';
import Database from 'better-sqlite3';
import { createServiceNowDocsClient, DOCS_LIMITS } from '../src/docs/github-client.js';
import { createDocsStore } from '../src/docs/sqlite-store.js';
import { MAX_REPORTED_SKIPS, parseMarkdownLinks, syncDocsFamily } from '../src/docs/sync.js';
import { mainIndex, startFakeRawGitHub } from './helpers/docs-fake-github.js';

const RAW = 'https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs';
const INDEX_PATH = '/ServiceNow/ServiceNowDocs/main/llms.txt';
const FAMILY_LLMS = '/ServiceNow/ServiceNowDocs/australia/llms.txt';
const doc = (p, branch = 'australia') => `/ServiceNow/ServiceNowDocs/${branch}/${p}`;

let fake;

afterEach(async () => {
  await fake?.close();
  fake = undefined;
});

async function setup(routes, { families, limits } = {}) {
  fake = await startFakeRawGitHub({ [INDEX_PATH]: mainIndex(families), ...routes });
  const cacheDir = await fs.mkdtemp(path.join(os.tmpdir(), 'happy-docs-sync-'));
  const client = createServiceNowDocsClient({ fetchImpl: fake.fetchImpl, limits });
  return { cacheDir, client };
}

function trackingStoreFactory() {
  const stats = { opened: 0, closed: 0 };
  const factory = async (...args) => {
    const store = await createDocsStore(...args);
    stats.opened += 1;
    const close = store.close.bind(store);
    store.close = () => {
      stats.closed += 1;
      close();
    };
    return store;
  };
  return { factory, stats };
}

async function exists(file) {
  return fs.access(file).then(() => true, () => false);
}

const cachedFile = (cacheDir, ...parts) => path.join(cacheDir, 'files', ...parts);

describe('parseMarkdownLinks', () => {
  test('accepts nested relative and same-repository same-branch raw links', () => {
    const result = parseMarkdownLinks([
      '- [Relative](platform/relative.md)',
      `- [Raw](${RAW}/australia/markdown/pub/nested/raw.md) -- description`,
      '- [Duplicate](platform/relative.md)',
      '- [Site](https://www.servicenow.com/docs)',
      '- [Family](https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/zurich/llms.txt)'
    ].join('\n'), { branch: 'australia' });

    expect(result.links).toEqual(['platform/relative.md', 'markdown/pub/nested/raw.md']);
    expect(result.rejectedCount).toBe(0);
  });

  test('rejects cross-repository, cross-branch, encoded, traversal, query, fragment and control links', () => {
    const targets = [
      'https://raw.githubusercontent.com/attacker/ServiceNowDocs/australia/x.md',
      'https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/zurich/x.md',
      'http://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/australia/x.md',
      '//evil.example/ServiceNow/ServiceNowDocs/australia/x.md',
      'https://evil.example/x.md',
      '/ServiceNow/other/main/x.md',
      'markdown%2F..%2F..%2Fx.md',
      'markdown/%2e%2e/x.md',
      '../outside.md',
      'markdown/x.md?ref=zurich',
      'markdown/x.md#top',
      'markdown/x\u0001.md'
    ];
    const result = parseMarkdownLinks(targets.map((t, i) => `- [L${i}](${t})`).join('\n'), { branch: 'australia' });

    expect(result.links).toEqual([]);
    expect(result.rejectedCount).toBe(targets.length);
    expect(result.rejected.map((r) => r.reason)).toEqual(expect.arrayContaining([
      'cross-repository', 'cross-branch', 'unsupported-origin', 'invalid-path'
    ]));
  });

  test('bounds rejected-link reporting', () => {
    const lines = Array.from({ length: MAX_REPORTED_SKIPS + 25 }, (_, i) => `- [x](../bad${i}.md)`);
    const result = parseMarkdownLinks(lines.join('\n'), { branch: 'australia' });

    expect(result.rejectedCount).toBe(MAX_REPORTED_SKIPS + 25);
    expect(result.rejected).toHaveLength(MAX_REPORTED_SKIPS);
  });

  test('fails clearly when the link count exceeds the cap instead of truncating', () => {
    const lines = Array.from({ length: 6 }, (_, i) => `- [x](d${i}.md)`);
    expect(() => parseMarkdownLinks(lines.join('\n'), { branch: 'australia', maxLinks: 5 }))
      .toThrow(/more than 5 documents/);
  });

  test('scans adversarial bracket-heavy input in linear time', () => {
    const hostile = [
      `${'['.repeat(20_000)}](${')'.repeat(20_000)}`,
      `${'x'.repeat(200)}](`.repeat(200) + ')',
      '[a]('.repeat(10_000),
      `${'](x)'.repeat(10_000)}`
    ].join('\n');
    const started = performance.now();
    parseMarkdownLinks(hostile, { branch: 'australia' });
    expect(performance.now() - started).toBeLessThan(500);
  });
});

describe('syncDocsFamily', () => {
  test('downloads relative and raw links from the approved branch and indexes them', async () => {
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: `- [Flow](platform/flow-designer.md)\n- [Raw](${RAW}/australia/markdown/pub/raw.md)`,
      [doc('platform/flow-designer.md')]: '# Flow Designer\n\nCreate actions.',
      [doc('markdown/pub/raw.md')]: '# Raw\n\nNested.'
    });

    const result = await syncDocsFamily({ family: 'australia', cacheDir, client });

    expect(result).toMatchObject({ family: 'australia', branch: 'australia', documentsSynced: 2, documentsSkipped: 0 });
    expect(await fs.readFile(cachedFile(cacheDir, 'australia', 'platform', 'flow-designer.md'), 'utf8'))
      .toContain('Create actions');
    const store = await createDocsStore(path.join(cacheDir, 'index.sqlite'));
    store.initialize();
    expect(store.getDocument({ family: 'australia', path: 'markdown/pub/raw.md' })).toMatchObject({ branch: 'australia' });
    store.close();
  });

  test('never requests rejected links and reports them', async () => {
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: [
        '- [Good](platform/good.md)',
        `- [Other branch](${RAW}/zurich/platform/zurich.md)`,
        '- [Encoded](platform%2F..%2F..%2Fescape.md)'
      ].join('\n'),
      [doc('platform/good.md')]: '# Good'
    });

    const result = await syncDocsFamily({ family: 'australia', cacheDir, client });

    expect(result.documentsSynced).toBe(1);
    expect(result.linksRejected).toBe(2);
    expect(fake.paths()).toEqual([INDEX_PATH, FAMILY_LLMS, doc('platform/good.md')]);
  });

  test('rejects a branch override that differs from the approved ref before touching the cache', async () => {
    const { cacheDir, client } = await setup({});
    const { factory, stats } = trackingStoreFactory();

    await expect(syncDocsFamily({ family: 'australia', branch: 'zurich', cacheDir, client, storeFactory: factory }))
      .rejects.toThrow(/does not match the approved ref/);
    await expect(syncDocsFamily({ family: 'australia', branch: '../main', cacheDir, client, storeFactory: factory }))
      .rejects.toThrow(/Invalid ServiceNow docs branch/);
    expect(stats.opened).toBe(0);
    expect(fake.paths()).toEqual([INDEX_PATH]);
    expect(await exists(cachedFile(cacheDir, 'australia'))).toBe(false);
  });

  test('accepts a branch override equal to the approved ref', async () => {
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: '- [Good](good.md)',
      [doc('good.md')]: '# Good'
    });

    await expect(syncDocsFamily({ family: 'australia', branch: 'australia', cacheDir, client }))
      .resolves.toMatchObject({ documentsSynced: 1 });
  });

  test('rejects families missing from the validated index', async () => {
    const { cacheDir, client } = await setup({});
    await expect(syncDocsFamily({ family: 'attacker', cacheDir, client })).rejects.toThrow(/Unknown ServiceNow docs family/);
  });

  test('fails clearly on too many links before allocating the store', async () => {
    const links = Array.from({ length: 4 }, (_, i) => `- [d](d${i}.md)`).join('\n');
    const { cacheDir, client } = await setup({ [FAMILY_LLMS]: links });
    const { factory, stats } = trackingStoreFactory();

    await expect(syncDocsFamily({
      family: 'australia', cacheDir, client, storeFactory: factory, limits: { ...DOCS_LIMITS, maxLinks: 3 }
    })).rejects.toThrow(/more than 3 documents/);
    expect(stats.opened).toBe(0);
    expect(fake.paths()).toEqual([INDEX_PATH, FAMILY_LLMS]);
  });

  test('aborts on the aggregate byte budget and closes the store', async () => {
    const body = `# Doc\n${'a'.repeat(500)}\n`;
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: '- [a](a.md)\n- [b](b.md)\n- [c](c.md)',
      [doc('a.md')]: body,
      [doc('b.md')]: body,
      [doc('c.md')]: body
    });
    const { factory, stats } = trackingStoreFactory();

    await expect(syncDocsFamily({
      family: 'australia', cacheDir, client, storeFactory: factory,
      limits: { ...DOCS_LIMITS, syncBytes: 1200 }
    })).rejects.toMatchObject({ code: 'DOCS_SYNC_BUDGET' });
    expect(stats).toEqual({ opened: 1, closed: 1 });
    expect(await exists(cachedFile(cacheDir, 'australia', 'c.md'))).toBe(false);
  });

  test('skips oversized and long-line documents with bounded reasons and keeps syncing', async () => {
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: '- [big](big.md)\n- [line](line.md)\n- [ok](ok.md)',
      [doc('big.md')]: `# Big\n${'b\n'.repeat(700)}`,
      [doc('line.md')]: `# Line\n${'l'.repeat(400)}\n`,
      [doc('ok.md')]: '# Ok'
    }, { limits: { ...DOCS_LIMITS, documentBytes: 1024, lineBytes: 256 } });

    const result = await syncDocsFamily({ family: 'australia', cacheDir, client });

    expect(result.documentsSynced).toBe(1);
    expect(result.skippedDocuments).toEqual([
      { path: 'big.md', error: expect.stringMatching(/exceeds 1024 bytes/) },
      { path: 'line.md', error: expect.stringMatching(/line longer than 256 bytes/) }
    ]);
    expect(await exists(cachedFile(cacheDir, 'australia', 'big.md'))).toBe(false);
  });

  test('bounds the skipped-document report', async () => {
    const count = MAX_REPORTED_SKIPS + 5;
    const links = Array.from({ length: count }, (_, i) => `- [m](missing${i}.md)`);
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: [...links, '- [ok](ok.md)'].join('\n'),
      [doc('ok.md')]: '# Ok'
    });

    const result = await syncDocsFamily({ family: 'australia', cacheDir, client });

    expect(result.documentsSkipped).toBe(count);
    expect(result.skippedDocuments).toHaveLength(MAX_REPORTED_SKIPS);
    expect(result.skippedDocumentsTruncated).toBe(true);
    expect(result.skippedDocuments[0].error).toMatch(/HTTP 404/);
  });

  test('closes the store when the sync deadline expires', async () => {
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: '- [slow](slow.md)',
      [doc('slow.md')]: () => {}
    });
    const { factory, stats } = trackingStoreFactory();

    await expect(syncDocsFamily({
      family: 'australia', cacheDir, client, storeFactory: factory,
      limits: { ...DOCS_LIMITS, syncTimeoutMs: 150 }
    })).rejects.toMatchObject({ code: 'DOCS_SYNC_DEADLINE' });
    expect(stats).toEqual({ opened: 1, closed: 1 });
  });

  test('does not open the store when the family index fetch fails', async () => {
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: (req, res) => { res.writeHead(500); res.end('boom ghp_secret'); }
    });
    const { factory, stats } = trackingStoreFactory();

    const error = await syncDocsFamily({ family: 'australia', cacheDir, client, storeFactory: factory })
      .catch((caught) => caught);
    expect(error.message).toMatch(/HTTP 500/);
    expect(error.message).not.toMatch(/ghp_secret/);
    expect(stats.opened).toBe(0);
  });

  test('fails when every document is skipped and still closes the store', async () => {
    const { cacheDir, client } = await setup({ [FAMILY_LLMS]: '- [m](missing.md)' });
    const { factory, stats } = trackingStoreFactory();

    await expect(syncDocsFamily({ family: 'australia', cacheDir, client, storeFactory: factory }))
      .rejects.toThrow(/all 1 documents were skipped/);
    expect(stats).toEqual({ opened: 1, closed: 1 });
  });

  test('invalidates cached content when the approved ref for a family changes', async () => {
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: '- [old](old.md)',
      [doc('old.md')]: '# Old branch content'
    });
    await syncDocsFamily({ family: 'australia', cacheDir, client });
    await fake.close();

    fake = await startFakeRawGitHub({
      [INDEX_PATH]: mainIndex([['australia', 'australia-2']]),
      [doc('llms.txt', 'australia-2')]: '- [new](new.md)',
      [doc('new.md', 'australia-2')]: '# New branch content'
    });
    const result = await syncDocsFamily({
      family: 'australia', cacheDir, client: createServiceNowDocsClient({ fetchImpl: fake.fetchImpl })
    });

    expect(result).toMatchObject({ branch: 'australia-2', documentsSynced: 1 });
    const store = await createDocsStore(path.join(cacheDir, 'index.sqlite'));
    store.initialize();
    expect(store.getDocument({ family: 'australia', path: 'old.md' })).toBeUndefined();
    expect(store.search({ query: 'Old', family: 'australia' })).toEqual([]);
    expect(store.getDocument({ family: 'australia', path: 'new.md' })).toMatchObject({ branch: 'australia-2' });
    store.close();
    expect(await exists(cachedFile(cacheDir, 'australia', 'old.md'))).toBe(false);
  });

  test('removes documents that disappeared from the family index on resync', async () => {
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: '- [a](a.md)\n- [b](b.md)',
      [doc('a.md')]: '# A',
      [doc('b.md')]: '# B'
    });
    await syncDocsFamily({ family: 'australia', cacheDir, client });
    fake.routes[FAMILY_LLMS] = '- [a](a.md)';

    await syncDocsFamily({
      family: 'australia', cacheDir, client: createServiceNowDocsClient({ fetchImpl: fake.fetchImpl })
    });

    const store = await createDocsStore(path.join(cacheDir, 'index.sqlite'));
    store.initialize();
    expect(store.getDocument({ family: 'australia', path: 'b.md' })).toBeUndefined();
    expect(store.getDocument({ family: 'australia', path: 'a.md' })).toBeDefined();
    store.close();
    expect(await exists(cachedFile(cacheDir, 'australia', 'b.md'))).toBe(false);
  });

  test('family cache directories can never collide with or delete the index database', async () => {
    const { cacheDir, client } = await setup({
      [doc('llms.txt', 'index.sqlite')]: '- [a](a.md)',
      [doc('a.md', 'index.sqlite')]: '# Reserved name',
      [FAMILY_LLMS]: '- [keep](keep.md)',
      [doc('keep.md')]: '# Keep me'
    }, { families: [['australia', 'australia'], ['index.sqlite', 'index.sqlite']] });
    await syncDocsFamily({ family: 'australia', cacheDir, client });
    await expect(syncDocsFamily({ family: 'index.sqlite', cacheDir, client }))
      .resolves.toMatchObject({ documentsSynced: 1 });
    await fake.close();

    fake = await startFakeRawGitHub({
      [INDEX_PATH]: mainIndex([['australia', 'australia'], ['index.sqlite', 'moved']]),
      [doc('llms.txt', 'moved')]: '- [b](b.md)',
      [doc('b.md', 'moved')]: '# Moved'
    });
    await syncDocsFamily({
      family: 'index.sqlite', cacheDir, client: createServiceNowDocsClient({ fetchImpl: fake.fetchImpl })
    });

    expect(await exists(path.join(cacheDir, 'index.sqlite'))).toBe(true);
    const store = await createDocsStore(path.join(cacheDir, 'index.sqlite'));
    store.initialize();
    expect(store.getDocument({ family: 'australia', path: 'keep.md' })).toMatchObject({ branch: 'australia' });
    expect(store.getDocument({ family: 'index.sqlite', path: 'b.md' })).toMatchObject({ branch: 'moved' });
    store.close();
  });

  test('cache write failures are reported without local filesystem paths and close the store', async () => {
    const { cacheDir, client } = await setup({
      [FAMILY_LLMS]: '- [x](platform/x.md)',
      [doc('platform/x.md')]: '# X'
    });
    await fs.mkdir(cachedFile(cacheDir, 'australia'), { recursive: true });
    await fs.writeFile(cachedFile(cacheDir, 'australia', 'platform'), 'not a directory');
    const { factory, stats } = trackingStoreFactory();

    const error = await syncDocsFamily({ family: 'australia', cacheDir, client, storeFactory: factory })
      .catch((caught) => caught);

    expect(error).toMatchObject({ code: 'DOCS_CACHE_WRITE' });
    expect(error.message).toMatch(/australia\/platform\/x\.md/);
    expect(error.message).not.toContain(cacheDir);
    expect(error.message).not.toContain(os.tmpdir());
    expect(stats).toEqual({ opened: 1, closed: 1 });
  });

  test('discards a legacy cache without branch provenance, including FTS and vector rows', async () => {
    const cacheDir = await fs.mkdtemp(path.join(os.tmpdir(), 'happy-docs-legacy-'));
    const dbPath = path.join(cacheDir, 'index.sqlite');
    const legacy = new Database(dbPath);
    legacy.exec(`
      CREATE TABLE families (name TEXT PRIMARY KEY, branch TEXT NOT NULL, synced_at TEXT);
      CREATE TABLE documents (id INTEGER PRIMARY KEY AUTOINCREMENT, family TEXT NOT NULL, path TEXT NOT NULL,
        sha TEXT, title TEXT, markdown TEXT NOT NULL, UNIQUE(family, path));
      CREATE TABLE chunks (id INTEGER PRIMARY KEY AUTOINCREMENT, document_id INTEGER NOT NULL, family TEXT NOT NULL,
        path TEXT NOT NULL, title TEXT, heading TEXT, start_line INTEGER, end_line INTEGER, body TEXT NOT NULL,
        FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE);
      CREATE VIRTUAL TABLE chunks_fts USING fts5(title, heading, body, content='chunks', content_rowid='id');
      INSERT INTO families VALUES ('australia', 'attacker-branch', '2026-01-01');
      INSERT INTO documents (family, path, title, markdown) VALUES ('australia', 'x.md', 'X', 'poisoned');
      INSERT INTO chunks (document_id, family, path, title, heading, start_line, end_line, body)
        VALUES (1, 'australia', 'x.md', 'X', 'X', 1, 1, 'poisoned payload');
      INSERT INTO chunks_fts (rowid, title, heading, body) VALUES (1, 'X', 'X', 'poisoned payload');
    `);
    const sqliteVec = await import('sqlite-vec').catch(() => null);
    const vectorConfig = { enableVector: true, embeddingProvider: 'local' };
    if (sqliteVec) {
      const { createLocalEmbedding, serializeVector } = await import('../src/docs/vector-index.js');
      sqliteVec.load(legacy);
      legacy.exec('CREATE VIRTUAL TABLE docs_chunk_vectors USING vec0(embedding float[128])');
      legacy.prepare('INSERT INTO docs_chunk_vectors(_rowid_, embedding) VALUES (?, ?)')
        .run(1n, serializeVector(createLocalEmbedding('poisoned payload')));
    }
    legacy.close();

    const store = await createDocsStore(dbPath, { vectorConfig });
    store.initialize();
    expect(store.getDocument({ family: 'australia', path: 'x.md' })).toBeUndefined();
    expect(store.status().families).toEqual([]);
    expect(store.search({ query: 'poisoned', family: 'australia' })).toEqual([]);
    store.close();

    const raw = new Database(dbPath, { readonly: true });
    expect(raw.prepare("SELECT count(*) AS n FROM chunks_fts WHERE chunks_fts MATCH 'poisoned'").get().n).toBe(0);
    expect(raw.prepare('SELECT count(*) AS n FROM chunks').get().n).toBe(0);
    if (sqliteVec) {
      sqliteVec.load(raw);
      expect(raw.prepare('SELECT count(*) AS n FROM docs_chunk_vectors').get().n).toBe(0);
    }
    raw.close();
  });
});
