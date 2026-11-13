#!/usr/bin/env node
// Generates fixtures/identity-v1-vectors.json from the JS implementation
// (src/servicenow-client.js:542-555). Goldens are immutable once committed:
// the script refuses to overwrite an existing file unless --force is passed.
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { oauthRefreshTokenAccount } from '../../../src/servicenow-client.js';
import { resolveOAuthEndpoints } from '../../../src/oauth-endpoint-policy.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const out = path.join(here, '..', 'fixtures', 'identity-v1-vectors.json');
if (fs.existsSync(out) && !process.argv.includes('--force')) {
  console.error(`${out} exists; goldens are immutable (use --force only with owner approval)`);
  process.exit(1);
}

const inputs = [
  { name: 'default-endpoints', url: 'https://dev451274.service-now.com', clientId: 'abc123', osUser: 'alice' },
  {
    name: 'normalisation-and-external-idp',
    url: 'HTTPS://Example.Service-Now.com:443/sub/path//',
    clientId: 'client-ß',
    authorizeUrl: 'https://login.example.com/oauth2/v2.0/authorize?x=1&y=%7E',
    tokenUrl: 'https://login.example.com:443/oauth2/v2.0/token',
    osUser: 'bob'
  },
  {
    name: 'unicode-and-json-escapes',
    url: 'https://dev451274.service-now.com:8443/',
    clientId: 'id"with\\quote\ttab\u0001',
    authorizeUrl: '',
    tokenUrl: 'https://idp.example.com/tëst path/token',
    osUser: 'Ünï cødé☃\u2028\u007f'
  },
  { name: 'idn-host', url: 'https://dév.example.com/', clientId: 'c', osUser: 'carol' },
  { name: 'windows-domain-user', url: 'https://tenant.example.com/path/', clientId: 'cid', osUser: 'CORP\\dave' }
];

const vectors = inputs.map(({ name, osUser, ...instance }) => {
  const account = oauthRefreshTokenAccount(instance, { osUser });
  // Re-derive the hashed string independently so the fixture records the exact bytes.
  const canonical = (() => {
    const u = new URL(instance.url);
    return `${u.origin}${u.pathname.replace(/\/+$/, '')}`;
  })();
  const ep = resolveOAuthEndpoints(canonical, instance);
  const tuple = JSON.stringify([
    'happy-platform-mcp/oauth-refresh-token', 1, osUser, canonical, instance.clientId,
    new URL(ep.authorizeUrl).href, new URL(ep.tokenUrl).href
  ]);
  const check = 'identity-v1-' + createHash('sha256').update(tuple, 'utf8').digest('hex');
  if (check !== account) throw new Error(`self-check failed for ${name}`);
  return { name, input: { ...instance, osUser }, tupleJson: tuple, account };
});

fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, JSON.stringify({ source: 'src/servicenow-client.js:542-555', vectors }, null, 2) + '\n');
console.log(`wrote ${vectors.length} vectors -> ${out}`);
