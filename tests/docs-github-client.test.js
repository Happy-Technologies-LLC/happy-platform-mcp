import { afterEach, describe, expect, test } from '@jest/globals';
import {
  createServiceNowDocsClient,
  DOCS_LIMITS,
  parseFamiliesFromLlms
} from '../src/docs/github-client.js';
import { mainIndex, startFakeRawGitHub } from './helpers/docs-fake-github.js';

const INDEX_PATH = '/ServiceNow/ServiceNowDocs/main/llms.txt';
const DOC_PATH = '/ServiceNow/ServiceNowDocs/australia/markdown/flow/designer.md';

let fake;
const originalEnv = { ...process.env };

afterEach(async () => {
  process.env = { ...originalEnv };
  await fake?.close();
  fake = undefined;
});

async function clientFor(routes, limits) {
  fake = await startFakeRawGitHub({ [INDEX_PATH]: mainIndex(), ...routes });
  return createServiceNowDocsClient({ fetchImpl: fake.fetchImpl, limits });
}

describe('parseFamiliesFromLlms', () => {
  test('parses the upstream family-to-branch mapping list', () => {
    expect(parseFamiliesFromLlms(mainIndex([
      ['australia', 'australia'],
      ['zurich', 'zurich']
    ]))).toEqual([
      { name: 'australia', branch: 'australia' },
      { name: 'zurich', branch: 'zurich' }
    ]);
  });

  test('parses pinned family llms.txt links and ignores off-repository links', () => {
    const families = parseFamiliesFromLlms([
      '- [australia](https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/australia/llms.txt)',
      '- [washingtondc](https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/washingtondc/llms.txt)',
      '- [evil](https://raw.githubusercontent.com/attacker/ServiceNowDocs/evil/llms.txt)',
      '- [dup](https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/australia/llms.txt)'
    ].join('\n'));

    expect(families).toEqual([
      { name: 'australia', branch: 'australia' },
      { name: 'washingtondc', branch: 'washingtondc' }
    ]);
  });

  test('ignores mapping entries with unsafe family or branch names', () => {
    expect(parseFamiliesFromLlms([
      '- "../x" : "australia"',
      '- "ok" : "feature/x"',
      '- "ok2" : "a..b"',
      '- "zurich" : "zurich"'
    ].join('\n'))).toEqual([{ name: 'zurich', branch: 'zurich' }]);
  });
});

describe('ServiceNowDocs raw client', () => {
  test('fetches the pinned main index without ambient GitHub credentials', async () => {
    process.env.GITHUB_TOKEN = 'ghp_ambient_secret_value';
    const client = await clientFor({});

    expect(await client.listFamilies()).toEqual([{ name: 'australia', branch: 'australia' }]);
    expect(fake.paths()).toEqual([INDEX_PATH]);
    expect(fake.requests[0].headers.authorization).toBeUndefined();
  });

  test('resolves the approved branch from the index and encodes a nested path', async () => {
    const client = await clientFor({ [DOC_PATH]: '# Designer\n\nBody' });

    await expect(client.getMarkdown('australia', 'markdown/flow/designer.md')).resolves.toBe('# Designer\n\nBody');
    expect(fake.paths()).toEqual([INDEX_PATH, DOC_PATH]);
  });

  test('rejects families that are not in the validated main index before fetching documents', async () => {
    const client = await clientFor({});

    await expect(client.getMarkdown('attacker', 'markdown/x.md')).rejects.toThrow(/Unknown ServiceNow docs family/);
    await expect(client.getMarkdown('../main', 'markdown/x.md')).rejects.toThrow(/Invalid ServiceNow docs family/);
    expect(fake.paths().filter((p) => p !== INDEX_PATH)).toEqual([]);
  });

  test.each([
    '../secret.md',
    'markdown/../../other/x.md',
    '/markdown/x.md',
    'markdown%2F..%2Fx.md',
    'markdown/%2e%2e/x.md',
    'markdown\\x.md',
    'markdown/x.md?token=1',
    'markdown/x.md#frag',
    'markdown/x\u0000.md',
    'markdown/x\n.md',
    'https://raw.githubusercontent.com/attacker/repo/main/x.md',
    'markdown/x.txt',
    'markdown//x.md',
    `${'a/'.repeat(40)}x.md`
  ])('rejects unsafe document path %j without any request', async (unsafePath) => {
    const client = await clientFor({});

    await expect(client.getMarkdown('australia', unsafePath)).rejects.toThrow(/Unsafe docs path/);
    expect(fake.requests).toHaveLength(0);
  });

  test('rejects redirects instead of following them', async () => {
    const client = await clientFor({
      [DOC_PATH]: (req, res) => {
        res.writeHead(302, { location: '/ServiceNow/ServiceNowDocs/australia/markdown/other.md' });
        res.end();
      },
      '/ServiceNow/ServiceNowDocs/australia/markdown/other.md': '# followed'
    });

    await expect(client.getMarkdown('australia', 'markdown/flow/designer.md')).rejects.toThrow(/redirect/i);
    expect(fake.paths()).not.toContain('/ServiceNow/ServiceNowDocs/australia/markdown/other.md');
  });

  test('sanitizes HTTP failures without echoing response bodies or status text', async () => {
    const client = await clientFor({
      [DOC_PATH]: (req, res) => {
        res.writeHead(403, 'secret-status-text ghp_leak', { 'content-type': 'text/plain' });
        res.end('rate limited for token ghp_body_secret');
      }
    });

    const error = await client.getMarkdown('australia', 'markdown/flow/designer.md').catch((caught) => caught);
    expect(error.message).toMatch(/HTTP 403/);
    expect(error.message).not.toMatch(/ghp_|secret|rate limited/);
    expect(error.code).toBe('DOCS_HTTP_STATUS');
  });

  test('rejects an oversized Content-Length before reading the body', async () => {
    const client = await clientFor({
      [DOC_PATH]: (req, res) => {
        res.writeHead(200, { 'content-length': String(2048) });
        res.end('x'.repeat(2048));
      }
    }, { ...DOCS_LIMITS, documentBytes: 1024 });

    await expect(client.getMarkdown('australia', 'markdown/flow/designer.md'))
      .rejects.toMatchObject({ code: 'DOCS_TOO_LARGE' });
  });

  test('cancels a never-ending streamed body once it exceeds the byte limit', async () => {
    let written = 0;
    const client = await clientFor({
      [DOC_PATH]: (req, res) => {
        res.writeHead(200, { 'content-type': 'text/plain' });
        const pump = () => {
          while (!res.destroyed && res.write(`${'y'.repeat(1023)}\n`)) written += 1024;
          if (!res.destroyed) res.once('drain', pump);
        };
        pump();
      }
    }, { ...DOCS_LIMITS, documentBytes: 64 * 1024, requestTimeoutMs: 20_000 });

    const started = performance.now();
    await expect(client.getMarkdown('australia', 'markdown/flow/designer.md'))
      .rejects.toMatchObject({ code: 'DOCS_TOO_LARGE' });
    expect(performance.now() - started).toBeLessThan(5_000);
    expect(written).toBeLessThan(16 * 1024 * 1024);
  });

  test('rejects a line longer than the per-line limit', async () => {
    const client = await clientFor({
      [DOC_PATH]: `# ok\n${'z'.repeat(300)}\n`
    }, { ...DOCS_LIMITS, lineBytes: 256 });

    await expect(client.getMarkdown('australia', 'markdown/flow/designer.md'))
      .rejects.toMatchObject({ code: 'DOCS_LINE_TOO_LONG' });
  });

  test('enforces the index byte limit', async () => {
    fake = await startFakeRawGitHub({ [INDEX_PATH]: `${mainIndex()}${'#\n'.repeat(1024)}` });
    const client = createServiceNowDocsClient({
      fetchImpl: fake.fetchImpl,
      limits: { ...DOCS_LIMITS, indexBytes: 1024 }
    });

    await expect(client.listFamilies()).rejects.toMatchObject({ code: 'DOCS_TOO_LARGE' });
  });

  test('times out a stalled response body', async () => {
    const client = await clientFor({
      [DOC_PATH]: (req, res) => {
        res.writeHead(200, { 'content-type': 'text/plain' });
        res.write('# partial\n');
      }
    }, { ...DOCS_LIMITS, requestTimeoutMs: 100 });

    await expect(client.getMarkdown('australia', 'markdown/flow/designer.md'))
      .rejects.toMatchObject({ code: 'DOCS_TIMEOUT' });
  });

  test('uses production limits and exposes the documented caps', () => {
    expect(DOCS_LIMITS).toMatchObject({
      indexBytes: 1024 * 1024,
      documentBytes: 8 * 1024 * 1024,
      lineBytes: 16 * 1024,
      maxLinks: 1000,
      syncBytes: 64 * 1024 * 1024,
      requestTimeoutMs: 30_000,
      syncTimeoutMs: 600_000
    });
  });
});
