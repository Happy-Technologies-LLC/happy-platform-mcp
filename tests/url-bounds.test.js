/**
 * URL/path length caps and linear slash normalization (issue #67:
 * VULN-024..029, VULN-031). Long slash runs must be rejected or normalized
 * promptly at every entry point, and normalization must never let a request
 * leave the configured instance origin/path prefix.
 */
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, describe, expect, jest, test } from '@jest/globals';
import { InstanceRegistry, InstanceRegistryError, canonicalizeInstanceUrl } from '../src/instance-registry.js';
import { ConfigManager } from '../src/config-manager.js';
import { ServiceNowClient } from '../src/servicenow-client.js';
import { assertApprovedOAuthEndpoint } from '../src/oauth-endpoint-policy.js';
import { createMcpServer } from '../src/mcp-server-consolidated.js';
import { MAX_URL_LENGTH, trimLeadingSlashes, trimTrailingSlashes } from '../src/url-limits.js';

const ORIGIN = 'https://dev.service-now.com';
const SLASH_RUN = '/'.repeat(100_000);
const PROMPT_MS = 50;
const tempDirs = [];

afterEach(() => {
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
  jest.restoreAllMocks();
});

function timed(fn) {
  const start = performance.now();
  let outcome;
  try {
    outcome = { value: fn() };
  } catch (error) {
    outcome = { error };
  }
  return { ...outcome, ms: performance.now() - start };
}

function tempRegistry() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'happy-url-bounds-'));
  tempDirs.push(dir);
  const file = path.join(dir, 'instances.json');
  return { file, registry: new InstanceRegistry({ readPath: file, writePath: file }) };
}

const publicInstance = (url, extra = {}) => ({
  name: 'dev',
  url,
  authType: 'oauth',
  grantType: 'authorization_code',
  clientId: 'public-client',
  ...extra
});

describe('linear slash helpers', () => {
  test('trim 100k-slash runs promptly while preserving non-slash content', () => {
    const trailing = timed(() => trimTrailingSlashes(`${ORIGIN}/tenant${SLASH_RUN}`));
    expect(trailing.value).toBe(`${ORIGIN}/tenant`);
    expect(trailing.ms).toBeLessThan(PROMPT_MS);

    const leading = timed(() => trimLeadingSlashes(`${SLASH_RUN}api/now`));
    expect(leading.value).toBe('api/now');
    expect(leading.ms).toBeLessThan(PROMPT_MS);

    expect(trimTrailingSlashes('/')).toBe('');
    expect(trimTrailingSlashes(`${SLASH_RUN}a`)).toBe(`${SLASH_RUN}a`);
  });
});

describe('instance registry URL caps', () => {
  test('canonicalizeInstanceUrl rejects a 100k-slash URL promptly', () => {
    const result = timed(() => canonicalizeInstanceUrl(`${ORIGIN}${SLASH_RUN}x`));
    expect(result.ms).toBeLessThan(PROMPT_MS);
    expect(result.error).toBeInstanceOf(InstanceRegistryError);
    expect(result.error.message).toMatch(/4096/);
  });

  test('canonicalizes root and approved path prefixes deterministically within the cap', () => {
    const run = '/'.repeat(MAX_URL_LENGTH - ORIGIN.length - '/tenant'.length);
    const atCap = `${ORIGIN}/tenant${run}`;
    expect(atCap).toHaveLength(MAX_URL_LENGTH);
    const result = timed(() => canonicalizeInstanceUrl(atCap));
    expect(result.value).toBe(`${ORIGIN}/tenant`);
    expect(result.ms).toBeLessThan(PROMPT_MS);
    expect(canonicalizeInstanceUrl(`${ORIGIN}/`)).toBe(ORIGIN);
    expect(canonicalizeInstanceUrl(`${ORIGIN}///`)).toBe(ORIGIN);
  });

  test('validate accepts the 4096-character boundary and rejects 4097 before parsing', () => {
    const { registry } = tempRegistry();
    const pad = 'a'.repeat(MAX_URL_LENGTH - `${ORIGIN}/`.length);
    expect(() => registry.validate(publicInstance(`${ORIGIN}/${pad}`))).not.toThrow();
    expect(() => registry.validate(publicInstance(`${ORIGIN}/${pad}a`))).toThrow(/4096/);

    const slashes = timed(() => registry.validate(publicInstance(`${ORIGIN}${SLASH_RUN}`)));
    expect(slashes.ms).toBeLessThan(PROMPT_MS);
    expect(slashes.error).toBeInstanceOf(InstanceRegistryError);
  });

  test('rejects oversized OAuth endpoint URLs and callback paths', () => {
    const { registry } = tempRegistry();
    const oauth = {
      name: 'dev',
      url: ORIGIN,
      authType: 'oauth',
      grantType: 'authorization_code',
      clientId: 'client'
    };
    for (const field of ['authorizeUrl', 'tokenUrl']) {
      const result = timed(() => registry.validate({ ...oauth, [field]: `${ORIGIN}${SLASH_RUN}oauth_token.do` }));
      expect(result.ms).toBeLessThan(PROMPT_MS);
      expect(result.error?.details?.field).toBe(field);
    }
    const callback = timed(() => registry.validate({ ...oauth, callbackPath: `/callback${SLASH_RUN}` }));
    expect(callback.error?.details?.field).toBe('callbackPath');
  });

  test('rejects malformed URLs', () => {
    const { registry } = tempRegistry();
    for (const url of ['not a url', 'https://', `${ORIGIN}:99999`, 'https://user:pw@dev.service-now.com', 'ftp://dev.service-now.com']) {
      expect(() => registry.validate(publicInstance(url))).toThrow(InstanceRegistryError);
    }
  });

  test('registry load rejects a stored oversized URL', () => {
    const { file, registry } = tempRegistry();
    fs.writeFileSync(file, JSON.stringify({ version: 1, instances: [publicInstance(`${ORIGIN}${SLASH_RUN}`)] }));
    const result = timed(() => registry.load());
    expect(result.ms).toBeLessThan(PROMPT_MS * 4);
    expect(result.error).toBeInstanceOf(InstanceRegistryError);
  });
});

describe('environment URL caps', () => {
  const originalEnv = process.env;
  afterEach(() => {
    process.env = originalEnv;
  });

  test('SERVICENOW_INSTANCE_URL with a 100k-slash run is rejected promptly', () => {
    process.env = {
      ...originalEnv,
      SERVICENOW_INSTANCE_URL: `${ORIGIN}${SLASH_RUN}x`,
      SERVICENOW_USERNAME: 'admin',
      SERVICENOW_PASSWORD: 'unit-test-non-secret'
    };
    delete process.env.SERVICENOW_AUTH_TYPE;
    const result = timed(() => new ConfigManager().loadFromEnv());
    expect(result.ms).toBeLessThan(PROMPT_MS);
    expect(result.error?.message).toMatch(/4096/);
  });
});

describe('SN-Register-Instance URL caps', () => {
  test('schema advertises maxLength and runtime rejects oversized URLs without writing', async () => {
    const { file, registry } = tempRegistry();
    const server = await createMcpServer({ setProgressCallback() {} }, {
      configManager: { registry, getInstance: name => registry.get(name), listInstances: () => registry.list(), reload: () => registry.reload() },
      instanceRegistry: registry,
      credentialStore: { hasSecret: async () => true }
    });
    const { tools } = await server._requestHandlers.get('tools/list')({ method: 'tools/list', params: {} }, {});
    const schema = tools.find(tool => tool.name === 'SN-Register-Instance').inputSchema;
    for (const field of ['url', 'authorizeUrl', 'tokenUrl', 'callbackPath']) {
      expect(schema.properties[field].maxLength).toBe(MAX_URL_LENGTH);
    }

    const start = performance.now();
    const result = await server._requestHandlers.get('tools/call')({
      method: 'tools/call',
      params: {
        name: 'SN-Register-Instance',
        arguments: { name: 'dev', url: `${ORIGIN}${SLASH_RUN}x`, authType: 'oauth', grantType: 'authorization_code', clientId: 'client' }
      }
    }, {});
    expect(performance.now() - start).toBeLessThan(PROMPT_MS * 4);
    expect(result.isError).toBe(true);
    expect(fs.existsSync(file)).toBe(false);
  });
});

describe('ServiceNowClient entry point', () => {
  test('rejects oversized, malformed and non-HTTP instance URLs promptly', () => {
    for (const url of [`${ORIGIN}${SLASH_RUN}x`, 'not a url', 'ftp://dev.service-now.com', 'https://u:p@dev.service-now.com', `${ORIGIN}/?q=1`, undefined]) {
      const result = timed(() => new ServiceNowClient(url, 'admin', 'pw'));
      expect(result.ms).toBeLessThan(PROMPT_MS);
      expect(result.error).toBeInstanceOf(Error);
    }
  });

  test('normalizes trailing slash runs within the cap and preserves path prefixes', () => {
    const run = '/'.repeat(3000);
    const client = new ServiceNowClient(`${ORIGIN}/tenant${run}`, 'admin', 'pw');
    expect(client.instanceUrl).toBe(`${ORIGIN}/tenant`);
    expect(new ServiceNowClient(`${ORIGIN}${run}`, 'admin', 'pw').instanceUrl).toBe(ORIGIN);
  });

  test('credentials never cross origin or escape the path prefix after slash normalization', async () => {
    const client = new ServiceNowClient(`${ORIGIN}/tenant${'/'.repeat(3000)}`, 'admin', 'pw');
    const sent = [];
    client.client.defaults.adapter = async config => {
      sent.push(config);
      return { data: { result: [] }, status: 200, statusText: 'OK', headers: {}, config };
    };
    await client.getRecords('incident');
    expect(sent).toHaveLength(1);
    expect(sent[0].headers.Authorization).toBeDefined();

    for (const url of ['https://evil.example.com/api/now/table/incident', '//evil.example.com/x', `${ORIGIN}/other/api`, `${SLASH_RUN}evil.example.com/api`]) {
      sent.length = 0;
      const start = performance.now();
      await expect(client.client.get(url)).rejects.toThrow();
      expect(performance.now() - start).toBeLessThan(PROMPT_MS * 4);
      expect(sent).toHaveLength(0);
    }
  });
});

describe('OAuth endpoint policy caps', () => {
  test('rejects oversized endpoint or instance URLs promptly', () => {
    for (const [endpoint, instance] of [
      [`${ORIGIN}${SLASH_RUN}oauth_token.do`, ORIGIN],
      [`${ORIGIN}/oauth_token.do`, `${ORIGIN}${SLASH_RUN}`]
    ]) {
      const result = timed(() => assertApprovedOAuthEndpoint(endpoint, instance, 'tokenUrl', {}));
      expect(result.ms).toBeLessThan(PROMPT_MS);
      expect(result.error?.code).toBe('OAUTH_ENDPOINT_NOT_APPROVED');
    }
  });
});
