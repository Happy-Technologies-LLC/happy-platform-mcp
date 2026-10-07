import os from 'os';
import fs from 'fs';
import path from 'path';
import { describe, expect, test, beforeEach, afterEach } from '@jest/globals';
import {
  assertDocsFamilyName,
  getDocsConfig,
  normalizeDocsDocumentPath,
  resolveDocsCachePath
} from '../src/docs/config.js';

const originalEnv = { ...process.env };

beforeEach(() => {
  process.env = { ...originalEnv };
  process.env.HAPPY_CONFIG_PATH = path.join(os.tmpdir(), `happy-docs-missing-${process.pid}.json`);
});

afterEach(() => {
  process.env = originalEnv;
});

describe('docs config', () => {
  test('uses the default cache directory under the user home', () => {
    delete process.env.HAPPY_DOCS_CACHE_DIR;
    const config = getDocsConfig();
    expect(config.cacheDir).toBe(path.join(os.homedir(), '.happy-platform-mcp', 'docs', 'servicenow'));
    expect(config.localIndexEnabled).toBe(false);
    expect(config.enableVector).toBe(false);
  });

  test('allows cache directory and vector flag through env vars', () => {
    process.env.HAPPY_DOCS_CACHE_DIR = '/tmp/happy-docs';
    process.env.HAPPY_DOCS_ENABLE_LOCAL_INDEX = 'true';
    process.env.HAPPY_DOCS_ENABLE_VECTOR = 'true';
    const config = getDocsConfig();
    expect(config.cacheDir).toBe('/tmp/happy-docs');
    expect(config.localIndexEnabled).toBe(true);
    expect(config.enableVector).toBe(true);
  });

  test('loads docs system properties from the local config file', () => {
    const configPath = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'happy-docs-config-')), 'config.json');
    fs.writeFileSync(configPath, JSON.stringify({
      docs: {
        cacheDir: '/tmp/from-config',
        localIndexEnabled: true,
        enableVector: true,
        embeddingProvider: 'local',
        githubToken: 'ghp_from_config'
      }
    }));
    process.env.HAPPY_CONFIG_PATH = configPath;

    process.env.GITHUB_TOKEN = 'ghp_ambient';
    const config = getDocsConfig();

    expect(config).toEqual({
      cacheDir: '/tmp/from-config',
      localIndexEnabled: true,
      enableVector: true,
      embeddingProvider: 'local'
    });
  });

  test('never surfaces ambient GitHub credentials in docs config', () => {
    process.env.GITHUB_TOKEN = 'ghp_ambient';
    expect(JSON.stringify(getDocsConfig())).not.toContain('ghp_');
  });

  test('normalizes safe nested document paths and rejects unsafe ones', () => {
    expect(normalizeDocsDocumentPath('markdown/pub/nested/foo.md')).toBe('markdown/pub/nested/foo.md');
    for (const unsafe of ['../secret.md', '/absolute.md', 'a/./b.md', 'a/%2e%2e/b.md', 'a\\b.md', 'C:/x.md', 'a/b.md?x', 'a b.md', '', 42]) {
      expect(() => normalizeDocsDocumentPath(unsafe)).toThrow(/Unsafe docs path/);
    }
    expect(() => normalizeDocsDocumentPath(`${'x'.repeat(1100)}.md`)).toThrow(/Unsafe docs path/);
  });

  test('validates family names', () => {
    expect(assertDocsFamilyName('australia')).toBe('australia');
    for (const unsafe of ['', '../x', 'a/b', 'a..b', '.hidden', 'x'.repeat(101), null]) {
      expect(() => assertDocsFamilyName(unsafe)).toThrow(/Invalid ServiceNow docs family/);
    }
  });

  test('resolves family cache paths under a dedicated files directory, never the index database', () => {
    const fullPath = resolveDocsCachePath('/tmp/cache', 'australia', 'foo/bar.md');
    expect(fullPath).toBe(path.join('/tmp/cache', 'files', 'australia', 'foo', 'bar.md'));
    expect(resolveDocsCachePath('/tmp/cache', 'australia')).toBe(path.join('/tmp/cache', 'files', 'australia'));
    expect(resolveDocsCachePath('/tmp/cache', 'index.sqlite')).toBe(path.join('/tmp/cache', 'files', 'index.sqlite'));
    expect(() => resolveDocsCachePath('/tmp/cache', '..', 'x.md')).toThrow(/Invalid ServiceNow docs family/);
  });
});
