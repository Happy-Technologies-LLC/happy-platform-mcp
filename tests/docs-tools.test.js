import fs from 'fs/promises';
import os from 'os';
import path from 'path';
import { afterEach, describe, expect, jest, test } from '@jest/globals';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { createMcpServer } from '../src/mcp-server-consolidated.js';
import { createServiceNowDocsClient } from '../src/docs/github-client.js';
import { docsToolDefinitions } from '../src/docs/tool-definitions.js';
import { handleDocsTool } from '../src/docs/tool-handlers.js';
import { mainIndex, startFakeRawGitHub } from './helpers/docs-fake-github.js';

const INDEX_PATH = '/ServiceNow/ServiceNowDocs/main/llms.txt';
const AU = '/ServiceNow/ServiceNowDocs/australia';
const originalEnv = { ...process.env };
let fake;

afterEach(async () => {
  process.env = { ...originalEnv };
  jest.restoreAllMocks();
  await fake?.close();
  fake = undefined;
});

async function tempDir(prefix) {
  return fs.mkdtemp(path.join(os.tmpdir(), prefix));
}

function docsConfig(cacheDir, localIndexEnabled) {
  return { cacheDir, localIndexEnabled, enableVector: false, embeddingProvider: 'none' };
}

function parseToolResponse(response) {
  return JSON.parse(response.content[0].text);
}

describe('docs MCP tools', () => {
  test('defines initial docs tools', () => {
    expect(docsToolDefinitions.map((tool) => tool.name)).toEqual([
      'SN-Docs-Families',
      'SN-Docs-Status',
      'SN-Docs-Sync',
      'SN-Docs-Search',
      'SN-Docs-Get'
    ]);
  });

  test('adds docs tools to consolidated MCP tool list', async () => {
    const server = await createMcpServer({ setProgressCallback() {} });
    const handler = server._requestHandlers.get('tools/list');
    const result = await handler({ method: 'tools/list', params: {} }, {});
    const docsTools = result.tools.filter((tool) => tool.name.startsWith('SN-Docs-'));

    expect(docsTools.map((tool) => tool.name)).toEqual([
      'SN-Docs-Families',
      'SN-Docs-Status',
      'SN-Docs-Sync',
      'SN-Docs-Search',
      'SN-Docs-Get'
    ]);
  });

  test('can expose only docs tools without a ServiceNow client', async () => {
    const server = await createMcpServer(null, { docsOnly: true });
    const handler = server._requestHandlers.get('tools/list');
    const result = await handler({ method: 'tools/list', params: {} }, {});

    expect(result.tools.map((tool) => tool.name)).toEqual([
      'SN-Register-Instance',
      'SN-Docs-Families',
      'SN-Docs-Status',
      'SN-Docs-Sync',
      'SN-Docs-Search',
      'SN-Docs-Get'
    ]);
  });

  test('defaults docs sync to the australia family resolved through the validated index', async () => {
    fake = await startFakeRawGitHub({
      [INDEX_PATH]: mainIndex(),
      [`${AU}/llms.txt`]: '- [Flow](platform/flow.md)',
      [`${AU}/platform/flow.md`]: '# Flow\n\nCreate actions.'
    });
    const cacheDir = await tempDir('happy-docs-default-');

    const response = await handleDocsTool('SN-Docs-Sync', {}, {
      config: docsConfig(cacheDir, true),
      client: createServiceNowDocsClient({ fetchImpl: fake.fetchImpl })
    });

    expect(parseToolResponse(response)).toMatchObject({ family: 'australia', branch: 'australia', documentsSynced: 1 });
  });

  test('direct SN-Docs-Get works with local indexing disabled and never touches the cache', async () => {
    fake = await startFakeRawGitHub({
      [INDEX_PATH]: mainIndex(),
      [`${AU}/platform/example.md`]: '# Australia docs'
    });
    const cacheDir = path.join(await tempDir('happy-docs-get-'), 'never-created');

    const response = await handleDocsTool('SN-Docs-Get', { path: 'platform/example.md' }, {
      config: docsConfig(cacheDir, false),
      client: createServiceNowDocsClient({ fetchImpl: fake.fetchImpl })
    });

    expect(parseToolResponse(response)).toEqual({
      source: 'github',
      document: { family: 'australia', branch: 'australia', path: 'platform/example.md', markdown: '# Australia docs' }
    });
    await expect(fs.access(cacheDir)).rejects.toMatchObject({ code: 'ENOENT' });
  });

  test('SN-Docs-Get rejects unsafe paths before cache or network use', async () => {
    const cacheDir = path.join(await tempDir('happy-docs-unsafe-'), 'never-created');
    const client = { getMarkdown: jest.fn() };

    for (const unsafe of ['../../etc/passwd.md', 'a%2F..%2Fb.md', 'x.md?y', undefined]) {
      await expect(handleDocsTool('SN-Docs-Get', { path: unsafe }, { config: docsConfig(cacheDir, true), client }))
        .rejects.toThrow(/Unsafe docs path/);
    }
    await expect(handleDocsTool('SN-Docs-Get', { family: '../x', path: 'a.md' }, { config: docsConfig(cacheDir, true), client }))
      .rejects.toThrow(/Invalid ServiceNow docs family/);
    expect(client.getMarkdown).not.toHaveBeenCalled();
    await expect(fs.access(cacheDir)).rejects.toMatchObject({ code: 'ENOENT' });
  });

  test('SN-Docs-Get serves synced documents from the local cache with provenance', async () => {
    fake = await startFakeRawGitHub({
      [INDEX_PATH]: mainIndex(),
      [`${AU}/llms.txt`]: '- [Flow](platform/flow.md)',
      [`${AU}/platform/flow.md`]: '# Flow\n\nCached.'
    });
    const cacheDir = await tempDir('happy-docs-cache-');
    const config = docsConfig(cacheDir, true);
    await handleDocsTool('SN-Docs-Sync', {}, { config, client: createServiceNowDocsClient({ fetchImpl: fake.fetchImpl }) });
    const offline = { getMarkdown: jest.fn() };

    const response = await handleDocsTool('SN-Docs-Get', { path: 'platform/flow.md' }, { config, client: offline });

    expect(parseToolResponse(response)).toMatchObject({
      source: 'local-cache',
      document: { family: 'australia', branch: 'australia', path: 'platform/flow.md' }
    });
    expect(offline.getMarkdown).not.toHaveBeenCalled();
  });

  test('SN-Docs-Get through a real MCP client returns controlled markdown without ambient credentials', async () => {
    fake = await startFakeRawGitHub({
      [INDEX_PATH]: mainIndex(),
      [`${AU}/markdown/pub/nested/page.md`]: '# Nested page\n\nControlled content.'
    });
    process.env.HAPPY_CONFIG_PATH = path.join(os.tmpdir(), `happy-docs-missing-${process.pid}.json`);
    process.env.HAPPY_DOCS_ENABLE_LOCAL_INDEX = 'false';
    process.env.GITHUB_TOKEN = 'ghp_ambient_should_not_be_sent';
    jest.spyOn(globalThis, 'fetch').mockImplementation(fake.fetchImpl);

    const server = await createMcpServer(null, { docsOnly: true });
    const client = new Client({ name: 'docs-smoke', version: '1.0.0' });
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    await server.connect(serverTransport);
    await client.connect(clientTransport);
    try {
      const ok = await client.callTool({
        name: 'SN-Docs-Get',
        arguments: { family: 'australia', path: 'markdown/pub/nested/page.md' }
      });
      expect(ok.isError).toBeFalsy();
      expect(JSON.parse(ok.content[0].text).document.markdown).toBe('# Nested page\n\nControlled content.');

      const denied = await client.callTool({ name: 'SN-Docs-Get', arguments: { path: '../../../etc/passwd.md' } });
      expect(denied.isError).toBe(true);
      expect(denied.content[0].text).toMatch(/Unsafe docs path/);

      const unknown = await client.callTool({ name: 'SN-Docs-Get', arguments: { family: 'attacker', path: 'x.md' } });
      expect(unknown.isError).toBe(true);
      expect(unknown.content[0].text).toMatch(/Unknown ServiceNow docs family/);
    } finally {
      await client.close();
      await server.close();
    }

    expect(fake.paths()).toEqual([INDEX_PATH, `${AU}/markdown/pub/nested/page.md`, INDEX_PATH]);
    expect(fake.requests.every((request) => request.headers.authorization === undefined)).toBe(true);
  });
});
