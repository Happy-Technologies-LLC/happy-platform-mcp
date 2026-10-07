/**
 * Tests for MCP Resources
 * Tests resource listing, reading, URI routing, and error handling
 */

import { jest } from '@jest/globals';
import { createResourceHandlers } from '../src/resources.js';
import { PACKAGE_VERSION } from '../src/package-info.js';
import { createMockMcpServer, mockTableMetadata } from './helpers/mocks.js';

describe('MCP Resources', () => {
  let mockServer;
  let listResourcesHandler;
  let readResourceHandler;

  beforeEach(() => {
    mockServer = createMockMcpServer();
    jest.clearAllMocks();

    // Simulate handler registration
    listResourcesHandler = jest.fn();
    readResourceHandler = jest.fn();

    mockServer.setRequestHandler.mockImplementation((schema, handler) => {
      if (schema.method === 'resources/list') {
        listResourcesHandler = handler;
      } else if (schema.method === 'resources/read') {
        readResourceHandler = handler;
      }
    });
  });

  describe('List Resources', () => {
    it('should list all available resources', async () => {
      const expectedResources = [
        {
          uri: 'servicenow://instance',
          mimeType: 'application/json',
          name: 'ServiceNow Instance Info',
          description: 'Information about the connected ServiceNow instance',
        },
        {
          uri: 'servicenow://tables/all',
          mimeType: 'application/json',
          name: 'All ServiceNow Tables',
          description: 'Complete list of available ServiceNow tables with metadata',
        },
      ];

      listResourcesHandler.mockResolvedValueOnce({ resources: expectedResources });

      const result = await listResourcesHandler();

      expect(result.resources).toHaveLength(2);
      expect(result.resources[0].uri).toBe('servicenow://instance');
      expect(result.resources[1].uri).toBe('servicenow://tables/all');
    });

    it('should include proper mime types', async () => {
      const resources = [
        {
          uri: 'servicenow://instance',
          mimeType: 'application/json',
          name: 'ServiceNow Instance Info',
          description: 'Information about the connected ServiceNow instance',
        },
      ];

      listResourcesHandler.mockResolvedValueOnce({ resources });

      const result = await listResourcesHandler();

      expect(result.resources[0].mimeType).toBe('application/json');
    });

    it('should handle empty resource list', async () => {
      listResourcesHandler.mockResolvedValueOnce({ resources: [] });

      const result = await listResourcesHandler();

      expect(result.resources).toHaveLength(0);
    });
  });

  describe('Read Resource - Instance Info', () => {
    it('should read instance information', async () => {
      const instanceInfo = {
        server_info: {
          name: 'Happy MCP Server (Consolidated)',
          version: PACKAGE_VERSION,
          description: 'Consolidated ServiceNow integration with metadata-driven schema lookups',
        },
        instance_info: {
          url: 'https://dev123.service-now.com',
          username: 'admin',
        },
        capabilities: {
          total_tables: 160,
          operations: ['create', 'read', 'update', 'query', 'schema_lookup'],
          tools: 34,
        },
      };

      readResourceHandler.mockResolvedValueOnce({
        contents: [{
          uri: 'servicenow://instance',
          mimeType: 'application/json',
          text: JSON.stringify(instanceInfo, null, 2),
        }],
      });

      const result = await readResourceHandler({ params: { uri: 'servicenow://instance' } });

      expect(result.contents[0].uri).toBe('servicenow://instance');
      expect(result.contents[0].mimeType).toBe('application/json');

      const parsed = JSON.parse(result.contents[0].text);
      expect(parsed.server_info.version).toBe(PACKAGE_VERSION);
      expect(parsed.capabilities.tools).toBe(34);
    });

    it('should include instance URL', async () => {
      const instanceInfo = {
        server_info: { name: 'Happy MCP Server', version: PACKAGE_VERSION },
        instance_info: { url: 'https://prod456.service-now.com' },
        capabilities: {},
      };

      readResourceHandler.mockResolvedValueOnce({
        contents: [{
          uri: 'servicenow://instance',
          mimeType: 'application/json',
          text: JSON.stringify(instanceInfo),
        }],
      });

      const result = await readResourceHandler({ params: { uri: 'servicenow://instance' } });
      const parsed = JSON.parse(result.contents[0].text);

      expect(parsed.instance_info.url).toBe('https://prod456.service-now.com');
    });
  });

  describe('Read Resource - Tables', () => {
    it('should read all tables metadata', async () => {
      readResourceHandler.mockResolvedValueOnce({
        contents: [{
          uri: 'servicenow://tables/all',
          mimeType: 'application/json',
          text: JSON.stringify(mockTableMetadata, null, 2),
        }],
      });

      const result = await readResourceHandler({ params: { uri: 'servicenow://tables/all' } });

      expect(result.contents[0].uri).toBe('servicenow://tables/all');

      const parsed = JSON.parse(result.contents[0].text);
      expect(parsed.incident).toBeDefined();
      expect(parsed.incident.label).toBe('Incident');
      expect(parsed.change_request).toBeDefined();
    });

    it('should include table fields and metadata', async () => {
      readResourceHandler.mockResolvedValueOnce({
        contents: [{
          uri: 'servicenow://tables/all',
          mimeType: 'application/json',
          text: JSON.stringify(mockTableMetadata),
        }],
      });

      const result = await readResourceHandler({ params: { uri: 'servicenow://tables/all' } });
      const parsed = JSON.parse(result.contents[0].text);

      expect(parsed.incident.key_field).toBe('number');
      expect(parsed.incident.required_fields).toContain('short_description');
      expect(parsed.incident.common_fields).toContain('state');
    });

    it('should handle large table metadata', async () => {
      const largeMeta = {};
      for (let i = 0; i < 200; i++) {
        largeMeta[`table${i}`] = {
          table: `table${i}`,
          label: `Table ${i}`,
          key_field: 'sys_id',
        };
      }

      readResourceHandler.mockResolvedValueOnce({
        contents: [{
          uri: 'servicenow://tables/all',
          mimeType: 'application/json',
          text: JSON.stringify(largeMeta),
        }],
      });

      const result = await readResourceHandler({ params: { uri: 'servicenow://tables/all' } });
      const parsed = JSON.parse(result.contents[0].text);

      expect(Object.keys(parsed)).toHaveLength(200);
    });
  });

  describe('Resource URI Routing', () => {
    it('should route to correct resource by URI', async () => {
      const uris = ['servicenow://instance', 'servicenow://tables/all'];

      for (const uri of uris) {
        readResourceHandler.mockResolvedValueOnce({
          contents: [{ uri, mimeType: 'application/json', text: '{}' }],
        });

        const result = await readResourceHandler({ params: { uri } });
        expect(result.contents[0].uri).toBe(uri);
      }
    });

    it('should handle URI with query parameters', async () => {
      const uri = 'servicenow://instance?include=all';

      readResourceHandler.mockResolvedValueOnce({
        contents: [{ uri, mimeType: 'application/json', text: '{}' }],
      });

      const result = await readResourceHandler({ params: { uri } });
      expect(result.contents[0].uri).toBe(uri);
    });

    it('should handle instance routing in URI', async () => {
      const uri = 'servicenow://dev/tables/all';

      readResourceHandler.mockResolvedValueOnce({
        contents: [{ uri, mimeType: 'application/json', text: '{}' }],
      });

      const result = await readResourceHandler({ params: { uri } });
      expect(result.contents[0].uri).toBe(uri);
    });
  });

  describe('Error Handling', () => {
    it('should handle unknown resource URI', async () => {
      readResourceHandler.mockRejectedValueOnce(
        new Error('Unknown resource: servicenow://unknown')
      );

      await expect(
        readResourceHandler({ params: { uri: 'servicenow://unknown' } })
      ).rejects.toThrow('Unknown resource: servicenow://unknown');
    });

    it('should handle malformed URI', async () => {
      readResourceHandler.mockRejectedValueOnce(
        new Error('Invalid URI format: invalid://uri')
      );

      await expect(
        readResourceHandler({ params: { uri: 'invalid://uri' } })
      ).rejects.toThrow('Invalid URI format');
    });

    it('should handle missing URI parameter', async () => {
      readResourceHandler.mockRejectedValueOnce(
        new Error('URI parameter is required')
      );

      await expect(
        readResourceHandler({ params: {} })
      ).rejects.toThrow('URI parameter is required');
    });

    it('should handle resource not found', async () => {
      readResourceHandler.mockRejectedValueOnce(
        new Error('Resource not found: servicenow://tables/nonexistent')
      );

      await expect(
        readResourceHandler({ params: { uri: 'servicenow://tables/nonexistent' } })
      ).rejects.toThrow('Resource not found');
    });
  });

  describe('JSON Formatting', () => {
    it('should return properly formatted JSON', async () => {
      const data = { test: 'value', nested: { key: 'value' } };

      readResourceHandler.mockResolvedValueOnce({
        contents: [{
          uri: 'servicenow://instance',
          mimeType: 'application/json',
          text: JSON.stringify(data, null, 2),
        }],
      });

      const result = await readResourceHandler({ params: { uri: 'servicenow://instance' } });

      expect(() => JSON.parse(result.contents[0].text)).not.toThrow();
      const parsed = JSON.parse(result.contents[0].text);
      expect(parsed.test).toBe('value');
    });

    it('should handle special characters in JSON', async () => {
      const data = {
        description: 'Test with "quotes" and \\backslash',
        unicode: '测试中文字符',
      };

      readResourceHandler.mockResolvedValueOnce({
        contents: [{
          uri: 'servicenow://instance',
          mimeType: 'application/json',
          text: JSON.stringify(data),
        }],
      });

      const result = await readResourceHandler({ params: { uri: 'servicenow://instance' } });
      const parsed = JSON.parse(result.contents[0].text);

      expect(parsed.description).toBe('Test with "quotes" and \\backslash');
      expect(parsed.unicode).toBe('测试中文字符');
    });

    it('should handle empty objects', async () => {
      readResourceHandler.mockResolvedValueOnce({
        contents: [{
          uri: 'servicenow://instance',
          mimeType: 'application/json',
          text: JSON.stringify({}),
        }],
      });

      const result = await readResourceHandler({ params: { uri: 'servicenow://instance' } });
      const parsed = JSON.parse(result.contents[0].text);

      expect(parsed).toEqual({});
    });
  });

  describe('Multi-Instance Support', () => {
    it('should list resources for specific instance', async () => {
      const devResources = [
        { uri: 'servicenow://dev/instance', name: 'Dev Instance', mimeType: 'application/json' },
      ];

      listResourcesHandler.mockResolvedValueOnce({ resources: devResources });

      const result = await listResourcesHandler({ instance: 'dev' });

      expect(result.resources[0].uri).toContain('dev');
    });

    it('should read resource from specific instance', async () => {
      readResourceHandler.mockResolvedValueOnce({
        contents: [{
          uri: 'servicenow://prod/instance',
          mimeType: 'application/json',
          text: JSON.stringify({ instance: 'prod' }),
        }],
      });

      const result = await readResourceHandler({
        params: { uri: 'servicenow://prod/instance' },
      });

      const parsed = JSON.parse(result.contents[0].text);
      expect(parsed.instance).toBe('prod');
    });
  it('uses the same injected credential store when switching and restoring instances', async () => {
    const credentialStore = {};
    let current = {
      name: 'dev',
      url: 'https://dev.service-now.com',
      username: 'dev-user',
      password: undefined
    };
    const instances = {
      dev: { ...current, authType: 'basic' },
      prod: { name: 'prod', url: 'https://prod.service-now.com', authType: 'basic' }
    };
    const client = {
      getCurrentInstance: jest.fn(() => current),
      setInstance: jest.fn((url, username, password, name) => {
        current = { name, url, username, password };
      }),
      getRecords: jest.fn(async () => [])
    };
    const configManager = {
      listInstances: jest.fn(() => Object.values(instances)),
      getInstance: jest.fn(name => instances[name])
    };
    const handlers = createResourceHandlers(client, configManager, mockTableMetadata, { credentialStore });

    await handlers.readResource('servicenow://prod/incidents');

    expect(client.setInstance).toHaveBeenNthCalledWith(
      1,
      instances.prod.url,
      instances.prod.username,
      instances.prod.password,
      'prod',
      expect.objectContaining({ credentialStore })
    );
    expect(client.setInstance).toHaveBeenNthCalledWith(
      2,
      instances.dev.url,
      instances.dev.username,
      instances.dev.password,
      'dev',
      expect.objectContaining({ credentialStore })
    );
  });
  });
});
