/**
 * Happy MCP Server - MCP Resources Implementation
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 *
 * Provides read-only, cacheable access to ServiceNow data
 */

export function createResourceHandlers(serviceNowClient, configManager, tableMetadata) {
  /**
   * List all available resources
   */
  const listResources = async () => {
    const currentInstance = serviceNowClient.getCurrentInstance();
    const instances = configManager.listInstances();

    const resources = [
      // Instance management
      {
        uri: 'servicenow://instances',
        mimeType: 'application/json',
        name: 'ServiceNow Instances',
        description: 'List of all configured ServiceNow instances'
      },
      {
        uri: `servicenow://${currentInstance.name}/info`,
        mimeType: 'application/json',
        name: `Current Instance Info (${currentInstance.name})`,
        description: 'Information about the currently connected ServiceNow instance'
      },

      // Table metadata
      {
        uri: 'servicenow://tables',
        mimeType: 'application/json',
        name: 'Available Tables',
        description: 'Complete list of available ServiceNow tables with metadata'
      },

      // Data resources (using current instance)
      {
        uri: `servicenow://${currentInstance.name}/incidents`,
        mimeType: 'application/json',
        name: `Active Incidents (${currentInstance.name})`,
        description: 'List of active incidents from the current instance'
      },
      {
        uri: `servicenow://${currentInstance.name}/users`,
        mimeType: 'application/json',
        name: `Users (${currentInstance.name})`,
        description: 'List of users from the current instance'
      },
      {
        uri: `servicenow://${currentInstance.name}/update-sets`,
        mimeType: 'application/json',
        name: `Update Sets (${currentInstance.name})`,
        description: 'List of update sets from the current instance'
      },
      {
        uri: `servicenow://${currentInstance.name}/groups`,
        mimeType: 'application/json',
        name: `User Groups (${currentInstance.name})`,
        description: 'List of user groups from the current instance'
      },
      {
        uri: `servicenow://${currentInstance.name}/change-requests`,
        mimeType: 'application/json',
        name: `Change Requests (${currentInstance.name})`,
        description: 'List of change requests from the current instance'
      }
    ];

    // Add per-instance resources for all configured instances
    instances.forEach(instance => {
      if (instance.name !== currentInstance.name) {
        resources.push(
          {
            uri: `servicenow://${instance.name}/incidents`,
            mimeType: 'application/json',
            name: `Active Incidents (${instance.name})`,
            description: `List of active incidents from ${instance.name}`
          },
          {
            uri: `servicenow://${instance.name}/users`,
            mimeType: 'application/json',
            name: `Users (${instance.name})`,
            description: `List of users from ${instance.name}`
          },
          {
            uri: `servicenow://${instance.name}/update-sets`,
            mimeType: 'application/json',
            name: `Update Sets (${instance.name})`,
            description: `List of update sets from ${instance.name}`
          }
        );
      }
    });

    console.error(`📚 Listing ${resources.length} resources`);
    return { resources };
  };

  /**
   * Read a specific resource
   */
  const readResource = async (uri) => {
    console.error(`📖 Reading resource: ${uri}`);

    // Parse URI: servicenow://[instance]/[resource]/[id]
    const uriPattern = /^servicenow:\/\/([^\/]+)(?:\/(.+))?$/;
    const match = uri.match(uriPattern);

    if (!match) {
      throw new Error(`Invalid resource URI format: ${uri}. Expected: servicenow://[instance]/[resource]`);
    }

    const [, instanceOrResource, resourcePath] = match;

    // Helper function to format response with metadata
    const formatResource = (data, description = '') => {
      const timestamp = new Date().toISOString();
      const formattedData = {
        metadata: {
          timestamp,
          instance: serviceNowClient.getCurrentInstance().name,
          description,
          record_count: Array.isArray(data) ? data.length : (data ? 1 : 0)
        },
        data
      };

      return {
        contents: [{
          uri,
          mimeType: 'application/json',
          text: JSON.stringify(formattedData, null, 2)
        }]
      };
    };

    // Resource: servicenow://instances
    if (instanceOrResource === 'instances' && !resourcePath) {
      const instances = configManager.listInstances();
      return formatResource(instances, 'List of all configured ServiceNow instances');
    }

    // Resource: servicenow://tables
    if (instanceOrResource === 'tables' && !resourcePath) {
      const tableList = Object.entries(tableMetadata).map(([tableName, meta]) => ({
        table_name: tableName,
        label: meta.label,
        description: meta.description,
        key_field: meta.key_field,
        operations: meta.operations,
        package: meta.package
      }));
      return formatResource(tableList, 'Complete list of available ServiceNow tables with metadata');
    }

    // Instance-specific resources
    const instanceName = instanceOrResource;
    const resource = resourcePath;

    // Save current instance to restore later
    const originalInstance = serviceNowClient.getCurrentInstance();

    // Switch to requested instance if different
    if (instanceName !== originalInstance.name) {
      const instance = configManager.getInstance(instanceName);
      serviceNowClient.setInstance(instance.url, instance.username, instance.password, instance.name, {
        authType: instance.authType || 'basic',
        clientId: instance.clientId,
        clientSecret: instance.clientSecret,
        grantType: instance.grantType,
        scope: instance.scope
      });
    }

    try {
      // Resource: servicenow://[instance]/info
      if (resource === 'info') {
        const currentInstance = serviceNowClient.getCurrentInstance();
        const instanceConfig = configManager.getInstance(instanceName);
        const data = {
          instance: {
            name: currentInstance.name,
            url: currentInstance.url,
            description: instanceConfig.description || '',
            default: instanceConfig.default || false
          },
          server_info: {
            name: 'Happy MCP Server',
            version: '2.0.0',
            description: 'Multi-instance ServiceNow MCP server with resources'
          },
          capabilities: {
            total_tables: Object.keys(tableMetadata).length,
            total_tools: 34,
            operations: ['create', 'read', 'update', 'query', 'batch', 'workflow'],
            features: ['multi_instance', 'resources', 'background_scripts', 'update_sets']
          }
        };
        return formatResource(data, `Information about ${instanceName} instance`);
      }

      // Resource: servicenow://[instance]/incidents
      if (resource === 'incidents') {
        const incidents = await serviceNowClient.getRecords('incident', {
          sysparm_query: 'active=true',
          sysparm_limit: 25,
          sysparm_fields: 'number,short_description,state,priority,assigned_to,sys_created_on,sys_updated_on'
        });
        return formatResource(incidents, `Active incidents from ${instanceName}`);
      }

      // Resource: servicenow://[instance]/incidents/[number]
      if (resource && resource.startsWith('incidents/')) {
        const incidentNumber = resource.split('/')[1];
        const incidents = await serviceNowClient.getRecords('incident', {
          sysparm_query: `number=${incidentNumber}`,
          sysparm_limit: 1
        });
        if (incidents.length === 0) {
          throw new Error(`Incident ${incidentNumber} not found in ${instanceName}`);
        }
        return formatResource(incidents[0], `Incident ${incidentNumber} from ${instanceName}`);
      }

      // Resource: servicenow://[instance]/users
      if (resource === 'users') {
        const users = await serviceNowClient.getRecords('sys_user', {
          sysparm_query: 'active=true',
          sysparm_limit: 50,
          sysparm_fields: 'user_name,name,email,title,department,sys_created_on'
        });
        return formatResource(users, `Active users from ${instanceName}`);
      }

      // Resource: servicenow://[instance]/update-sets
      if (resource === 'update-sets') {
        const updateSets = await serviceNowClient.getRecords('sys_update_set', {
          sysparm_query: 'state=in progress',
          sysparm_limit: 25,
          sysparm_fields: 'name,description,state,application,sys_created_on,sys_updated_on',
          sysparm_order_by: '-sys_updated_on'
        });
        return formatResource(updateSets, `Update sets in progress from ${instanceName}`);
      }

      // Resource: servicenow://[instance]/update-sets/[sys_id]
      if (resource && resource.startsWith('update-sets/')) {
        const updateSetId = resource.split('/')[1];

        // Get update set details
        const updateSet = await serviceNowClient.getRecord('sys_update_set', updateSetId);

        // Get update set contents
        const updates = await serviceNowClient.getRecords('sys_update_xml', {
          sysparm_query: `update_set=${updateSetId}`,
          sysparm_fields: 'type,name,target_name,sys_created_on',
          sysparm_limit: 1000
        });

        // Group by type
        const typeGroups = {};
        updates.forEach(update => {
          const type = update.type || 'unknown';
          if (!typeGroups[type]) {
            typeGroups[type] = { count: 0, items: [] };
          }
          typeGroups[type].count++;
          typeGroups[type].items.push({
            name: update.name || update.target_name,
            created: update.sys_created_on
          });
        });

        const data = {
          update_set: {
            sys_id: updateSet.sys_id,
            name: updateSet.name,
            description: updateSet.description,
            state: updateSet.state,
            application: updateSet.application
          },
          total_records: updates.length,
          components: Object.entries(typeGroups).map(([type, info]) => ({
            type,
            count: info.count,
            items: info.items.slice(0, 10) // First 10 items per type
          }))
        };

        return formatResource(data, `Update set ${updateSet.name} contents from ${instanceName}`);
      }

      // Resource: servicenow://[instance]/groups
      if (resource === 'groups') {
        const groups = await serviceNowClient.getRecords('sys_user_group', {
          sysparm_query: 'active=true',
          sysparm_limit: 50,
          sysparm_fields: 'name,description,type,manager,sys_created_on'
        });
        return formatResource(groups, `Active user groups from ${instanceName}`);
      }

      // Resource: servicenow://[instance]/change-requests
      if (resource === 'change-requests') {
        const changes = await serviceNowClient.getRecords('change_request', {
          sysparm_query: 'active=true',
          sysparm_limit: 25,
          sysparm_fields: 'number,short_description,state,priority,risk,start_date,end_date,sys_created_on'
        });
        return formatResource(changes, `Active change requests from ${instanceName}`);
      }

      throw new Error(`Unknown resource path: ${resource}. Available resources: info, incidents, users, update-sets, groups, change-requests`);
    } finally {
      // Restore original instance if we switched
      if (instanceName !== originalInstance.name) {
        const original = configManager.getInstance(originalInstance.name);
        serviceNowClient.setInstance(original.url, original.username, original.password, original.name, {
          authType: original.authType || 'basic',
          clientId: original.clientId,
          clientSecret: original.clientSecret,
          grantType: original.grantType,
          scope: original.scope
        });
      }
    }
  };

  return {
    listResources,
    readResource
  };
}
