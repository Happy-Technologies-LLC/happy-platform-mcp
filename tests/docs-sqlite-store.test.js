import fs from 'fs/promises';
import os from 'os';
import path from 'path';
import { describe, expect, test, beforeEach } from '@jest/globals';
import { createDocsStore } from '../src/docs/sqlite-store.js';

let tempDir;

beforeEach(async () => {
  tempDir = await fs.mkdtemp(path.join(os.tmpdir(), 'happy-docs-'));
});

describe('docs sqlite store', () => {
  test('indexes and searches chunks with FTS5', async () => {
    const store = await createDocsStore(path.join(tempDir, 'index.sqlite'));
    store.initialize();
    store.beginFamilySync({ name: 'australia', branch: 'australia' });
    store.replaceDocument({
      family: 'australia',
      branch: 'australia',
      path: 'foo.md',
      sha: 'abc',
      title: 'Flow Designer',
      markdown: '# Flow Designer\n\nCreate actions.'
    }, [
      {
        family: 'australia',
        path: 'foo.md',
        title: 'Flow Designer',
        heading: 'Flow Designer',
        startLine: 1,
        endLine: 3,
        body: '# Flow Designer\n\nCreate actions.'
      }
    ]);

    const results = store.search({ query: 'create actions', family: 'australia', limit: 5 });
    expect(results).toHaveLength(1);
    expect(results[0]).toMatchObject({
      family: 'australia',
      path: 'foo.md',
      title: 'Flow Designer'
    });
    store.close();
  });

  test('refuses documents whose ref differs from the family approved ref', async () => {
    const store = await createDocsStore(path.join(tempDir, 'index.sqlite'));
    store.initialize();
    store.beginFamilySync({ name: 'australia', branch: 'australia' });

    expect(() => store.replaceDocument({
      family: 'australia', branch: 'zurich', path: 'x.md', sha: null, title: 'X', markdown: '# X'
    }, [])).toThrow(/provenance mismatch/);
    expect(store.getDocument({ family: 'australia', path: 'x.md' })).toBeUndefined();
    store.close();
  });

  test('reports status', async () => {
    const store = await createDocsStore(path.join(tempDir, 'index.sqlite'));
    store.initialize();
    expect(store.status()).toMatchObject({ ftsAvailable: true, vectorAvailable: false });
    store.close();
  });

  test('closes the database when vector initialization throws', async () => {
    let closed = 0;
    class FakeDatabase {
      close() { closed += 1; }
      loadExtension() { throw new Error('boom'); }
    }

    await expect(createDocsStore('ignored', {
      Database: FakeDatabase,
      vectorConfig: { enableVector: true, embeddingProvider: 'local', embedText: () => [], loadSqliteVec: async () => { throw new Error('load failed'); } }
    })).rejects.toThrow(/load failed/);
    expect(closed).toBe(1);
  });

  test('indexes and searches chunks with sqlite-vec when vector mode is enabled', async () => {
    const store = await createDocsStore(path.join(tempDir, 'vector.sqlite'), {
      vectorConfig: {
        enableVector: true,
        embeddingProvider: 'local'
      }
    });
    store.initialize();

    if (!store.status().vectorAvailable) {
      store.close();
      return;
    }

    store.beginFamilySync({ name: 'australia', branch: 'australia' });
    store.replaceDocument({
      family: 'australia',
      branch: 'australia',
      path: 'vector.md',
      sha: 'abc',
      title: 'Flow Designer',
      markdown: '# Flow Designer\n\nCreate reusable flow actions.'
    }, [
      {
        family: 'australia',
        path: 'vector.md',
        title: 'Flow Designer',
        heading: 'Flow Designer',
        startLine: 1,
        endLine: 3,
        body: '# Flow Designer\n\nCreate reusable flow actions.'
      }
    ]);

    const results = store.search({ query: 'reusable flow actions', family: 'australia', limit: 5 });
    expect(results[0]).toMatchObject({
      family: 'australia',
      path: 'vector.md',
      title: 'Flow Designer'
    });
    expect(results[0].vectorDistance).toEqual(expect.any(Number));
    store.close();
  });
});
