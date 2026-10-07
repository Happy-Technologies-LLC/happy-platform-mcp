import { jest } from '@jest/globals';
import crypto from 'node:crypto';
import express from 'express';
import { createDefaultClient, createHttpApp, validateHttpTransportSecurity } from '../src/http-server.js';

describe('validateHttpTransportSecurity', () => {
  test('rejects a network-visible HTTP listener without an access token', () => {
    expect(() => validateHttpTransportSecurity({ host: '0.0.0.0' })).toThrow(
      'HAPPY_MCP_API_TOKEN is required when HAPPY_MCP_BIND_HOST is not loopback'
    );
  });
});

async function requestHealth(app, headers = {}) {
  const server = app.listen(0, '127.0.0.1');
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address();
  try {
    return await fetch(`http://127.0.0.1:${port}/health`, { headers });
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
}

// Test sockets are always loopback, so spoof the peer address in front of the app.
function withRemoteAddress(app, remoteAddress) {
  const outer = express();
  outer.use((req, res, next) => {
    Object.defineProperty(req.socket, 'remoteAddress', { value: remoteAddress, configurable: true });
    next();
  });
  outer.use(app);
  return outer;
}

async function requestInstances(app, headers = {}) {
  const server = app.listen(0, '127.0.0.1');
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address();
  try {
    return await fetch(`http://127.0.0.1:${port}/instances`, { headers });
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
}

async function startApp(app) {
  const server = app.listen(0, '127.0.0.1');
  await new Promise((resolve) => server.once('listening', resolve));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    close: () => new Promise((resolve) => server.close(resolve))
  };
}

class FakeSseTransport {
  constructor(path, response) {
    this.path = path;
    this.response = response;
    this.sessionId = crypto.randomUUID();
  }
}

test('creates an authorization_code client with every OAuth option', () => {
  const client = createDefaultClient({
    name: 'public-oauth',
    url: 'https://example.service-now.com',
    authType: 'oauth',
    grantType: 'authorization_code',
    clientId: 'public-client',
    authorizeUrl: 'https://example.service-now.com/oauth_auth.do',
    tokenUrl: 'https://example.service-now.com/oauth_token.do',
    redirectPort: 8455,
    callbackPath: '/callback'
  });

  expect(client.oauthConfig).toMatchObject({
    grantType: 'authorization_code',
    clientId: 'public-client',
    authorizeUrl: 'https://example.service-now.com/oauth_auth.do',
    tokenUrl: 'https://example.service-now.com/oauth_token.do',
    redirectPort: 8455,
    callbackPath: '/callback'
  });
});

test('passes one injected credential store through default clients and MCP sessions', async () => {
  const credentialStore = {};
  const instance = {
    name: 'credentialed',
    url: 'https://example.service-now.com',
    authType: 'basic',
    username: 'user',
    credentialRef: 'keychain:instance/credentialed/password'
  };
  const client = createDefaultClient(instance, { credentialStore });
  expect(client._credentialStore).toBe(credentialStore);

  const createServiceNowClient = jest.fn(() => client);
  const createMcpServer = jest.fn(async () => ({
    connect: async transport => transport.response.write('data: connected\n\n')
  }));
  const app = createHttpApp({
    defaultInstance: instance,
    credentialStore,
    createServiceNowClient,
    createMcpServer,
    SSEServerTransport: FakeSseTransport
  });
  const server = await startApp(app);
  try {
    const response = await fetch(`${server.url}/mcp`);
    await response.body.cancel();
  } finally {
    await server.close();
  }

  expect(createServiceNowClient).toHaveBeenCalledWith(instance, { credentialStore });
  expect(createMcpServer).toHaveBeenCalledWith(client, { credentialStore });
});
test('passes canonical config manager and registry to every HTTP MCP session', async () => {
  const configManager = { listInstances: jest.fn() };
  const instanceRegistry = { list: jest.fn() };
  const credentialStore = {};
  const instance = { name: 'test', url: 'https://example.service-now.com' };
  const client = {};
  const createServiceNowClient = jest.fn(() => client);
  const createMcpServer = jest.fn(async () => ({
    connect: async transport => transport.response.write('data: connected\n\n')
  }));
  const app = createHttpApp({
    defaultInstance: instance,
    configManager,
    instanceRegistry,
    credentialStore,
    createServiceNowClient,
    createMcpServer,
    SSEServerTransport: FakeSseTransport
  });
  const server = await startApp(app);

  try {
    const response = await fetch(`${server.url}/mcp`);
    await response.body.cancel();
  } finally {
    await server.close();
  }

  expect(createMcpServer).toHaveBeenCalledWith(client, {
    configManager,
    instanceRegistry,
    credentialStore
  });
});

describe('GET /instances embedded access guard', () => {
  const instances = [{ name: 'dev', url: 'https://dev.example.service-now.com', default: true }];
  const createApp = (options = {}) => createHttpApp({
    defaultInstance: instances[0],
    listInstances: () => instances,
    ...options
  });

  test.each([
    ['remote IPv4', '203.0.113.5'],
    ['remote IPv6', '2001:db8::1'],
    ['unknown (undefined)', undefined],
    ['unknown (empty)', '']
  ])('rejects %s without a token', async (_label, remoteAddress) => {
    const response = await requestInstances(withRemoteAddress(createApp(), remoteAddress));

    expect(response.status).toBe(401);
    expect(await response.json()).toEqual({ error: 'Unauthorized' });
  });

  test.each([
    ['IPv4 loopback', '127.0.0.1'],
    ['IPv6 loopback', '::1'],
    ['IPv4-mapped IPv6 loopback', '::ffff:127.0.0.1']
  ])('still allows %s without a token', async (_label, remoteAddress) => {
    const response = await requestInstances(withRemoteAddress(createApp(), remoteAddress));

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ instances });
  });

  describe('with a configured token', () => {
    const apiToken = 'release-secret';

    test.each([
      ['no credentials', {}],
      ['a wrong token', { Authorization: 'Bearer wrong-secret' }]
    ])('rejects %s', async (_label, headers) => {
      const response = await requestInstances(createApp({ apiToken }), headers);

      expect(response.status).toBe(401);
    });

    test('accepts the bearer token, including from a non-loopback address', async () => {
      const app = withRemoteAddress(createApp({ apiToken }), '203.0.113.5');

      const response = await requestInstances(app, { Authorization: `Bearer ${apiToken}` });

      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({ instances });
    });
  });
});

describe('HTTP authorization', () => {
  test('rejects unauthenticated requests when an API token is configured', async () => {
    const app = createHttpApp({
      apiToken: 'release-secret',
      defaultInstance: { name: 'test', url: 'https://example.service-now.com' }
    });

    const response = await requestHealth(app);

    expect(response.status).toBe(401);
  });

  test('accepts a request with the configured bearer token', async () => {
    const app = createHttpApp({
      apiToken: 'release-secret',
      defaultInstance: { name: 'test', url: 'https://example.service-now.com' }
    });

    const response = await requestHealth(app, {
      Authorization: 'Bearer release-secret'
    });

    expect(response.status).toBe(200);
  });

  test('lists every configured instance', async () => {
    const instances = [
      { name: 'dev', url: 'https://dev.example.service-now.com', default: true },
      { name: 'prod', url: 'https://prod.example.service-now.com', default: false }
    ];
    const app = createHttpApp({
      defaultInstance: instances[0],
      listInstances: () => instances
    });

    const response = await requestInstances(app);

    expect(await response.json()).toEqual({ instances });
  });

  test('creates an isolated ServiceNow client for every MCP session', async () => {
    const createServiceNowClient = jest.fn(() => ({}));
    const createMcpServer = jest.fn(async () => ({
      connect: async (transport) => transport.response.write('data: connected\n\n')
    }));
    const app = createHttpApp({
      defaultInstance: { name: 'test', url: 'https://example.service-now.com' },
      createServiceNowClient,
      createMcpServer,
      SSEServerTransport: FakeSseTransport
    });
    const server = await startApp(app);

    try {
      const first = await fetch(`${server.url}/mcp`);
      await first.body.cancel();
      const second = await fetch(`${server.url}/mcp`);
      await second.body.cancel();
    } finally {
      await server.close();
    }

    expect(createServiceNowClient).toHaveBeenCalledTimes(2);
    expect(createMcpServer.mock.calls[0][0]).not.toBe(createMcpServer.mock.calls[1][0]);
  });
});
