/**
 * Happy MCP Server - MCP Tool Registration
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 */

import { Server } from '@modelcontextprotocol/sdk/server/index.js';
import { ListToolsRequestSchema, CallToolRequestSchema, ListResourcesRequestSchema, ReadResourceRequestSchema } from '@modelcontextprotocol/sdk/types.js';
import fs from 'fs/promises';
import path from 'path';
import { configManager } from './config-manager.js';
import { syncScript, syncAllScripts, SCRIPT_TYPES } from './script-sync.js';
import { parseNaturalLanguage, getSupportedPatterns } from './natural-language.js';

export async function createMcpServer(serviceNowClient) {
  const server = new Server(
    {
      name: 'servicenow-server',
      version: '2.0.0',
    },
    {
      capabilities: {
        tools: {},
        resources: {},
        prompts: {}
      }
    }
  );

  // Set up progress callback for ServiceNow client
  serviceNowClient.setProgressCallback((message) => {
    try {
      server.notification({
        method: 'notifications/progress',
        params: {
          progress: message
        }
      });
    } catch (error) {
      console.error('Failed to send progress notification:', error.message);
    }
  });

  // Load table metadata
  let tableMetadata = {};
  try {
    const metadataPath = path.resolve(path.dirname(import.meta.url.replace('file://', '')), 'config/comprehensive-table-definitions.json');
    const rawData = await fs.readFile(metadataPath, 'utf-8');
    const fullData = JSON.parse(rawData);

    // Extract just the table definitions, filtering out metadata
    Object.entries(fullData).forEach(([key, value]) => {
      if (!key.startsWith('_') && typeof value === 'object' && value.table) {
        tableMetadata[key] = value;
      }
    });

    console.error(`✅ Loaded metadata for ${Object.keys(tableMetadata).length} ServiceNow tables`);
  } catch (error) {
    console.error('⚠️  Failed to load table metadata:', error.message);
  }

  // Set up consolidated tool handlers
  server.setRequestHandler(ListToolsRequestSchema, async () => {
    console.error(`📋 Tool list requested by Claude Code`);

    const tools = [
      {
        name: 'SN-Set-Instance',
        description: 'Switch to a different ServiceNow instance. Use this at the start of your session to target a specific instance (dev, test, prod, etc.). Lists available instances if no name provided.',
        inputSchema: {
          type: 'object',
          properties: {
            instance_name: {
              type: 'string',
              description: 'Name of the instance to switch to (e.g., "dev", "prod", "test"). Leave empty to list available instances.'
            }
          }
        }
      },
      {
        name: 'SN-Get-Current-Instance',
        description: 'Get information about the currently active ServiceNow instance',
        inputSchema: {
          type: 'object',
          properties: {}
        }
      },
      {
        name: 'SN-Query-Table',
        description: 'Query any ServiceNow table by name with flexible filtering',
        inputSchema: {
          type: 'object',
          properties: {
            table_name: {
              type: 'string',
              description: 'ServiceNow table name (e.g., "incident", "sys_user", "cmdb_ci") (required)'
            },
            query: {
              type: 'string',
              description: 'ServiceNow encoded query string (e.g., "state=1^priority=1") (optional)'
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            },
            limit: {
              type: 'number',
              description: 'Maximum number of records to return (default: 25)',
              default: 25
            },
            offset: {
              type: 'number',
              description: 'Number of records to skip for pagination (optional)'
            },
            order_by: {
              type: 'string',
              description: 'Field to sort by (e.g., "created_on" or "-priority" for descending) (optional)'
            }
          },
          required: ['table_name']
        }
      },
      {
        name: 'SN-Create-Record',
        description: 'Create a record in any ServiceNow table by name. WARNING: For catalog_ui_policy_action table, fields ui_policy and catalog_variable cannot be set via REST API - use SN-Execute-Background-Script with setValue() after creation.',
        inputSchema: {
          type: 'object',
          properties: {
            table_name: {
              type: 'string',
              description: 'ServiceNow table name (e.g., "incident", "sys_user", "cmdb_ci") (required)'
            },
            data: {
              type: 'object',
              description: 'Record data as key-value pairs (e.g., {"short_description": "Test", "priority": 1}) (required)'
            }
          },
          required: ['table_name', 'data']
        }
      },
      {
        name: 'SN-Get-Record',
        description: 'Get a specific record from any ServiceNow table by sys_id',
        inputSchema: {
          type: 'object',
          properties: {
            table_name: {
              type: 'string',
              description: 'ServiceNow table name (e.g., "incident", "sys_user", "cmdb_ci") (required)'
            },
            sys_id: {
              type: 'string',
              description: 'System ID of the record to retrieve (required)'
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            }
          },
          required: ['table_name', 'sys_id']
        }
      },
      {
        name: 'SN-Update-Record',
        description: 'Update a record in any ServiceNow table by sys_id',
        inputSchema: {
          type: 'object',
          properties: {
            table_name: {
              type: 'string',
              description: 'ServiceNow table name (e.g., "incident", "sys_user", "cmdb_ci") (required)'
            },
            sys_id: {
              type: 'string',
              description: 'System ID of the record to update (required)'
            },
            data: {
              type: 'object',
              description: 'Record data to update as key-value pairs (e.g., {"state": 6, "resolution_notes": "Fixed"}) (required)'
            }
          },
          required: ['table_name', 'sys_id', 'data']
        }
      },
      {
        name: 'SN-Get-Table-Schema',
        description: 'Get the schema/metadata for any ServiceNow table including required fields, common fields, and field descriptions',
        inputSchema: {
          type: 'object',
          properties: {
            table_name: {
              type: 'string',
              description: 'ServiceNow table name (required)'
            }
          },
          required: ['table_name']
        }
      },
      {
        name: 'SN-List-Available-Tables',
        description: 'List all available ServiceNow tables with their descriptions and capabilities',
        inputSchema: {
          type: 'object',
          properties: {
            category: {
              type: 'string',
              description: 'Filter by category: "core_itsm", "platform", "service_catalog", "cmdb", or "all" (optional)'
            }
          },
          required: []
        }
      },
      // Convenience tools for most common operations
      {
        name: 'SN-List-Incidents',
        description: 'List Incident records with filtering and pagination',
        inputSchema: {
          type: 'object',
          properties: {
            state: {
              type: 'string',
              description: 'Filter by state (e.g., "New", "In Progress", "Resolved") (optional)'
            },
            priority: {
              type: 'number',
              description: 'Filter by priority (1-5) (optional)'
            },
            query: {
              type: 'string',
              description: 'ServiceNow encoded query string (e.g., "state=1^priority=1") (optional)'
            },
            limit: {
              type: 'number',
              description: 'Maximum number of records to return (default: 25)',
              default: 25
            },
            offset: {
              type: 'number',
              description: 'Number of records to skip for pagination (optional)'
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            },
            order_by: {
              type: 'string',
              description: 'Field to sort by (e.g., "created_on" or "-priority" for descending) (optional)'
            }
          },
          required: []
        }
      },
      {
        name: 'SN-Create-Incident',
        description: 'Create a new Incident',
        inputSchema: {
          type: 'object',
          properties: {
            short_description: { type: 'string', description: 'short description (required)' },
            description: { type: 'string', description: 'description (optional)' },
            caller_id: { type: 'string', description: 'caller id (optional)' },
            category: { type: 'string', description: 'category (optional)' },
            subcategory: { type: 'string', description: 'subcategory (optional)' },
            urgency: { type: 'string', description: 'urgency (optional)' },
            impact: { type: 'string', description: 'impact (optional)' },
            priority: { type: 'string', description: 'priority (optional)' },
            assigned_to: { type: 'string', description: 'assigned to (optional)' },
            assignment_group: { type: 'string', description: 'assignment group (optional)' },
            state: { type: 'string', description: 'state (optional)' },
            work_notes: { type: 'string', description: 'work notes (optional)' },
            sys_created_by: { type: 'string', description: 'sys created by (optional)' },
            sys_created_on: { type: 'string', description: 'sys created on (optional)' },
            sys_updated_by: { type: 'string', description: 'sys updated by (optional)' },
            sys_updated_on: { type: 'string', description: 'sys updated on (optional)' }
          },
          required: ['short_description']
        }
      },
      {
        name: 'SN-Get-Incident',
        description: 'Get a Incident by ID',
        inputSchema: {
          type: 'object',
          properties: {
            sys_id: { type: 'string', description: 'System ID' }
          },
          required: ['sys_id']
        }
      },
      {
        name: 'SN-List-SysUsers',
        description: 'List Sys User records with filtering and pagination',
        inputSchema: {
          type: 'object',
          properties: {
            query: {
              type: 'string',
              description: 'ServiceNow encoded query string (e.g., "state=1^priority=1") (optional)'
            },
            limit: {
              type: 'number',
              description: 'Maximum number of records to return (default: 25)',
              default: 25
            },
            offset: {
              type: 'number',
              description: 'Number of records to skip for pagination (optional)'
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            },
            order_by: {
              type: 'string',
              description: 'Field to sort by (e.g., "created_on" or "-priority" for descending) (optional)'
            }
          },
          required: []
        }
      },
      {
        name: 'SN-List-CmdbCis',
        description: 'List Cmdb Ci records with filtering and pagination',
        inputSchema: {
          type: 'object',
          properties: {
            query: {
              type: 'string',
              description: 'ServiceNow encoded query string (e.g., "state=1^priority=1") (optional)'
            },
            limit: {
              type: 'number',
              description: 'Maximum number of records to return (default: 25)',
              default: 25
            },
            offset: {
              type: 'number',
              description: 'Number of records to skip for pagination (optional)'
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            },
            order_by: {
              type: 'string',
              description: 'Field to sort by (e.g., "created_on" or "-priority" for descending) (optional)'
            }
          },
          required: []
        }
      },
      {
        name: 'SN-List-SysUserGroups',
        description: 'List Sys User Group records with filtering and pagination',
        inputSchema: {
          type: 'object',
          properties: {
            query: {
              type: 'string',
              description: 'ServiceNow encoded query string (e.g., "state=1^priority=1") (optional)'
            },
            limit: {
              type: 'number',
              description: 'Maximum number of records to return (default: 25)',
              default: 25
            },
            offset: {
              type: 'number',
              description: 'Number of records to skip for pagination (optional)'
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            },
            order_by: {
              type: 'string',
              description: 'Field to sort by (e.g., "created_on" or "-priority" for descending) (optional)'
            }
          },
          required: []
        }
      },
      {
        name: 'SN-List-ChangeRequests',
        description: 'List Change Request records with filtering and pagination',
        inputSchema: {
          type: 'object',
          properties: {
            state: {
              type: 'string',
              description: 'Filter by change state (optional)'
            },
            type: {
              type: 'string',
              description: 'Filter by change type (e.g., "Normal", "Emergency") (optional)'
            },
            query: {
              type: 'string',
              description: 'ServiceNow encoded query string (e.g., "state=1^priority=1") (optional)'
            },
            limit: {
              type: 'number',
              description: 'Maximum number of records to return (default: 25)',
              default: 25
            },
            offset: {
              type: 'number',
              description: 'Number of records to skip for pagination (optional)'
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            },
            order_by: {
              type: 'string',
              description: 'Field to sort by (e.g., "created_on" or "-priority" for descending) (optional)'
            }
          },
          required: []
        }
      },
      {
        name: 'SN-Set-Update-Set',
        description: 'Generate a fix script to set the current update set using GlideUpdateSet API. Cannot be done via REST API - creates script file for manual execution in ServiceNow UI.',
        inputSchema: {
          type: 'object',
          properties: {
            update_set_sys_id: {
              type: 'string',
              description: 'System ID of the update set to make current (required)'
            }
          },
          required: ['update_set_sys_id']
        }
      },
      {
        name: 'SN-Get-Current-Update-Set',
        description: 'Get the currently active update set',
        inputSchema: {
          type: 'object',
          properties: {},
          required: []
        }
      },
      {
        name: 'SN-List-Update-Sets',
        description: 'List available update sets',
        inputSchema: {
          type: 'object',
          properties: {
            query: {
              type: 'string',
              description: 'ServiceNow encoded query string (e.g., "state=in progress") (optional)'
            },
            limit: {
              type: 'number',
              description: 'Maximum number of records to return (default: 25)',
              default: 25
            },
            offset: {
              type: 'number',
              description: 'Number of records to skip for pagination (optional)'
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            },
            order_by: {
              type: 'string',
              description: 'Field to sort by (e.g., "created_on" or "-sys_created_on" for descending) (optional)'
            }
          },
          required: []
        }
      },
      {
        name: 'SN-Set-Current-Application',
        description: 'Set the current application scope using the UI API. This changes which application is active for development and configuration changes.',
        inputSchema: {
          type: 'object',
          properties: {
            app_sys_id: {
              type: 'string',
              description: 'System ID of the application to make current (required)'
            }
          },
          required: ['app_sys_id']
        }
      },
      {
        name: 'SN-List-Problems',
        description: 'List Problem records with filtering and pagination',
        inputSchema: {
          type: 'object',
          properties: {
            query: {
              type: 'string',
              description: 'ServiceNow encoded query string (e.g., "state=1^priority=1") (optional)'
            },
            limit: {
              type: 'number',
              description: 'Maximum number of records to return (default: 25)',
              default: 25
            },
            offset: {
              type: 'number',
              description: 'Number of records to skip for pagination (optional)'
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            },
            order_by: {
              type: 'string',
              description: 'Field to sort by (e.g., "created_on" or "-priority" for descending) (optional)'
            }
          },
          required: []
        }
      },
      {
        name: 'SN-Natural-Language-Search',
        description: 'Search ServiceNow records using natural language queries. Converts human-readable queries into ServiceNow encoded queries and executes them. Supports: Priority (P1-P5, high/low), Assignment (assigned to me, unassigned, assigned to <name>), Dates (created today, last 7 days, recent), States (new/open/closed/in progress), Content (about SAP, containing error), Impact/Urgency (high/medium/low), Numbers (number is INC0012345). Examples: "find all P1 incidents", "show recent problems assigned to me", "high priority changes created last week", "open incidents about SAP", "unassigned P2 incidents created today". Returns both the parsed encoded query and matching records with pattern analysis.',
        inputSchema: {
          type: 'object',
          properties: {
            query: {
              type: 'string',
              description: 'Natural language query (e.g., "high priority incidents assigned to me", "recent problems about database") (required)'
            },
            table: {
              type: 'string',
              description: 'Target ServiceNow table name (default: "incident"). Common tables: incident, problem, change_request, sys_user, cmdb_ci',
              default: 'incident'
            },
            limit: {
              type: 'number',
              description: 'Maximum number of records to return (default: 25)',
              default: 25
            },
            fields: {
              type: 'string',
              description: 'Comma-separated list of fields to return (optional)'
            },
            order_by: {
              type: 'string',
              description: 'Field to sort by (e.g., "sys_created_on" or "-priority" for descending) (optional)'
            },
            show_patterns: {
              type: 'boolean',
              description: 'Include pattern matching details in response (default: true)',
              default: true
            }
          },
          required: ['query']
        }
      },
      {
        name: 'SN-Execute-Background-Script',
        description: '🚀 EXECUTES background scripts with THREE methods: (1) sys_trigger [DEFAULT & MOST RELIABLE] - Creates scheduled job that runs in 1 second and auto-deletes, (2) UI endpoint (sys.scripts.do) - Attempts direct execution via UI, (3) Fix script - Manual fallback. Use for: setting update sets, complex GlideRecord operations, GlideUpdateSet API calls, etc. The sys_trigger method is most reliable and works consistently!',
        inputSchema: {
          type: 'object',
          properties: {
            script: {
              type: 'string',
              description: 'JavaScript code to execute (required)'
            },
            description: {
              type: 'string',
              description: 'Description of what the script does (optional)'
            },
            execution_method: {
              type: 'string',
              description: 'Execution method: "trigger" (default - most reliable), "ui" (UI endpoint), "auto" (try trigger then ui then fix script)',
              enum: ['trigger', 'ui', 'auto'],
              default: 'trigger'
            }
          },
          required: ['script']
        }
      },
      {
        name: 'SN-Create-Fix-Script',
        description: '⚠️ CREATES (not executes) a fix script file for MANUAL execution. ServiceNow REST API does NOT support direct script execution. This tool generates a .js file in /scripts/ directory with full instructions and optional auto-delete flag. You MUST manually copy and run the script in ServiceNow UI: System Definition → Scripts - Background. Use for: linking UI Policy Actions, setting update sets, complex GlideRecord operations that cannot be done via REST API.',
        inputSchema: {
          type: 'object',
          properties: {
            script_name: {
              type: 'string',
              description: 'Name for the script file (e.g., "link_ui_policy_actions") (required)'
            },
            script_content: {
              type: 'string',
              description: 'JavaScript code content (required)'
            },
            description: {
              type: 'string',
              description: 'Description of what the script does (optional)'
            },
            auto_delete: {
              type: 'boolean',
              description: 'If true, script file will be deleted after you confirm execution (default: false)',
              default: false
            }
          },
          required: ['script_name', 'script_content']
        }
      },
      {
        name: 'SN-Discover-Table-Schema',
        description: 'Deep schema introspection with ServiceNow-specific metadata including type codes, choice tables, and relationships',
        inputSchema: {
          type: 'object',
          properties: {
            table_name: {
              type: 'string',
              description: 'ServiceNow table name (required)'
            },
            include_type_codes: {
              type: 'boolean',
              description: 'Show internal type codes (e.g., 1=Choice, 5=Select Box) (optional)',
              default: false
            },
            include_choice_tables: {
              type: 'boolean',
              description: 'Show which choice tables to use (optional)',
              default: false
            },
            include_relationships: {
              type: 'boolean',
              description: 'Show parent/child table relationships (optional)',
              default: false
            },
            include_ui_policies: {
              type: 'boolean',
              description: 'Show UI policies affecting this table (optional)',
              default: false
            },
            include_business_rules: {
              type: 'boolean',
              description: 'Show business rules for this table (optional)',
              default: false
            },
            include_field_constraints: {
              type: 'boolean',
              description: 'Show field validations and defaults (optional)',
              default: false
            }
          },
          required: ['table_name']
        }
      },
      {
        name: 'SN-Batch-Create',
        description: 'Create multiple related records in one operation with variable references and transactional support. Reports progress during execution.',
        inputSchema: {
          type: 'object',
          properties: {
            operations: {
              type: 'array',
              description: 'Array of create operations. Each operation can reference previous operations via ${save_as_name}',
              items: {
                type: 'object',
                properties: {
                  table: { type: 'string', description: 'Table name' },
                  data: { type: 'object', description: 'Record data' },
                  save_as: { type: 'string', description: 'Variable name to save sys_id as (optional)' }
                }
              }
            },
            transaction: {
              type: 'boolean',
              description: 'All-or-nothing transaction (default: true)',
              default: true
            },
            progress: {
              type: 'boolean',
              description: 'Report progress notifications (default: true)',
              default: true
            }
          },
          required: ['operations']
        }
      },
      {
        name: 'SN-Batch-Update',
        description: 'Update multiple records efficiently in a single operation. Reports progress during execution.',
        inputSchema: {
          type: 'object',
          properties: {
            updates: {
              type: 'array',
              description: 'Array of update operations',
              items: {
                type: 'object',
                properties: {
                  table: { type: 'string', description: 'Table name' },
                  sys_id: { type: 'string', description: 'Record sys_id' },
                  data: { type: 'object', description: 'Fields to update' }
                }
              }
            },
            stop_on_error: {
              type: 'boolean',
              description: 'Stop processing on first error (default: false)',
              default: false
            },
            progress: {
              type: 'boolean',
              description: 'Report progress notifications (default: true)',
              default: true
            }
          },
          required: ['updates']
        }
      },
      {
        name: 'SN-Explain-Field',
        description: 'Get comprehensive explanation of a specific field including type, constraints, and known issues',
        inputSchema: {
          type: 'object',
          properties: {
            table: {
              type: 'string',
              description: 'Table name (required)'
            },
            field: {
              type: 'string',
              description: 'Field name (required)'
            },
            include_examples: {
              type: 'boolean',
              description: 'Include usage examples (default: true)',
              default: true
            }
          },
          required: ['table', 'field']
        }
      },
      {
        name: 'SN-Validate-Configuration',
        description: 'Validate catalog item configuration including variables, UI policies, and business rules',
        inputSchema: {
          type: 'object',
          properties: {
            catalog_item: {
              type: 'string',
              description: 'Catalog item sys_id (required)'
            },
            checks: {
              type: 'object',
              description: 'Validation checks to perform',
              properties: {
                variables: {
                  type: 'object',
                  properties: {
                    check_linked: { type: 'boolean' },
                    check_types: { type: 'boolean' },
                    check_choices: { type: 'boolean' },
                    check_mandatory: { type: 'boolean' }
                  }
                },
                ui_policies: {
                  type: 'object',
                  properties: {
                    check_conditions: { type: 'boolean' },
                    check_actions_linked: { type: 'boolean' },
                    check_variables_exist: { type: 'boolean' }
                  }
                }
              }
            }
          },
          required: ['catalog_item']
        }
      },
      {
        name: 'SN-Inspect-Update-Set',
        description: 'Inspect update set contents and verify completeness',
        inputSchema: {
          type: 'object',
          properties: {
            update_set: {
              type: 'string',
              description: 'Update set sys_id (required)'
            },
            show_components: {
              type: 'boolean',
              description: 'Show component breakdown (default: true)',
              default: true
            },
            show_dependencies: {
              type: 'boolean',
              description: 'Show missing dependencies (default: false)',
              default: false
            }
          },
          required: ['update_set']
        }
      },
      {
        name: 'SN-Create-Workflow',
        description: 'Create a complete ServiceNow workflow with activities, transitions, and conditions. This tool orchestrates the entire workflow creation process: base workflow → version → activities → transitions → publish. Reports progress during creation.',
        inputSchema: {
          type: 'object',
          properties: {
            name: {
              type: 'string',
              description: 'Workflow name (required)'
            },
            description: {
              type: 'string',
              description: 'Workflow description (optional)'
            },
            table: {
              type: 'string',
              description: 'Table this workflow runs against (e.g., "incident", "change_request")'
            },
            condition: {
              type: 'string',
              description: 'Condition for workflow to trigger (e.g., "state=1^priority=1") (optional)'
            },
            activities: {
              type: 'array',
              description: 'Array of activity definitions',
              items: {
                type: 'object',
                properties: {
                  name: { type: 'string', description: 'Activity name' },
                  script: { type: 'string', description: 'JavaScript code to execute' },
                  activity_definition_sys_id: { type: 'string', description: 'Activity type sys_id (optional)' }
                },
                required: ['name']
              }
            },
            transitions: {
              type: 'array',
              description: 'Array of transition definitions (connects activities)',
              items: {
                type: 'object',
                properties: {
                  from: { type: 'string', description: 'From activity name' },
                  to: { type: 'string', description: 'To activity name' },
                  condition_script: { type: 'string', description: 'JavaScript condition (optional)' }
                },
                required: ['from', 'to']
              }
            },
            publish: {
              type: 'boolean',
              description: 'Publish workflow after creation (default: false)',
              default: false
            },
            progress: {
              type: 'boolean',
              description: 'Report progress notifications (default: true)',
              default: true
            }
          },
          required: ['name', 'table', 'activities']
        }
      },
      {
        name: 'SN-Create-Activity',
        description: 'Create a single workflow activity with embedded JavaScript code. Use this for adding activities to existing workflows.',
        inputSchema: {
          type: 'object',
          properties: {
            workflow_version_sys_id: {
              type: 'string',
              description: 'Workflow version sys_id (required)'
            },
            name: {
              type: 'string',
              description: 'Activity name (required)'
            },
            script: {
              type: 'string',
              description: 'JavaScript code to execute in this activity (optional)'
            },
            activity_definition_sys_id: {
              type: 'string',
              description: 'Activity type sys_id (optional - defaults to generic activity)'
            },
            x: {
              type: 'number',
              description: 'X coordinate on canvas (default: 100)'
            },
            y: {
              type: 'number',
              description: 'Y coordinate on canvas (default: 100)'
            }
          },
          required: ['workflow_version_sys_id', 'name']
        }
      },
      {
        name: 'SN-Create-Transition',
        description: 'Create a transition between two workflow activities with optional condition',
        inputSchema: {
          type: 'object',
          properties: {
            from_activity_sys_id: {
              type: 'string',
              description: 'From activity sys_id (required)'
            },
            to_activity_sys_id: {
              type: 'string',
              description: 'To activity sys_id (required)'
            },
            condition_script: {
              type: 'string',
              description: 'JavaScript condition for this transition (optional)'
            },
            order: {
              type: 'number',
              description: 'Transition order (default: 1)'
            }
          },
          required: ['from_activity_sys_id', 'to_activity_sys_id']
        }
      },
      {
        name: 'SN-Publish-Workflow',
        description: 'Publish a workflow version, setting the start activity and making it active',
        inputSchema: {
          type: 'object',
          properties: {
            version_sys_id: {
              type: 'string',
              description: 'Workflow version sys_id (required)'
            },
            start_activity_sys_id: {
              type: 'string',
              description: 'Starting activity sys_id (required)'
            }
          },
          required: ['version_sys_id', 'start_activity_sys_id']
        }
      },
      {
        name: 'SN-Move-Records-To-Update-Set',
        description: 'Move sys_update_xml records to a different update set. Supports filtering by sys_ids, time range, or source update set. Extremely useful when records end up in wrong update set (e.g., "Default" instead of custom set). Reports progress during move operation.',
        inputSchema: {
          type: 'object',
          properties: {
            update_set_id: {
              type: 'string',
              description: 'Target update set sys_id to move records to (required)'
            },
            record_sys_ids: {
              type: 'array',
              description: 'Array of sys_update_xml sys_ids to move (optional)',
              items: { type: 'string' }
            },
            time_range: {
              type: 'object',
              description: 'Time range to filter records (optional - format: YYYY-MM-DD HH:MM:SS)',
              properties: {
                start: { type: 'string', description: 'Start time (e.g., "2025-09-29 20:00:00")' },
                end: { type: 'string', description: 'End time (e.g., "2025-09-29 20:03:31")' }
              }
            },
            source_update_set: {
              type: 'string',
              description: 'Filter by source update set name (e.g., "Default") (optional)'
            },
            table: {
              type: 'string',
              description: 'Table name (default: sys_update_xml)',
              default: 'sys_update_xml'
            },
            progress: {
              type: 'boolean',
              description: 'Report progress notifications (default: true)',
              default: true
            }
          },
          required: ['update_set_id']
        }
      },
      {
        name: 'SN-Clone-Update-Set',
        description: 'Clone an entire update set with all its sys_update_xml records. Creates a complete copy for backup, testing, or branching development work. Reports progress during cloning operation.',
        inputSchema: {
          type: 'object',
          properties: {
            source_update_set_id: {
              type: 'string',
              description: 'Source update set sys_id to clone (required)'
            },
            new_name: {
              type: 'string',
              description: 'Name for the new cloned update set (required)'
            },
            progress: {
              type: 'boolean',
              description: 'Report progress notifications (default: true)',
              default: true
            }
          },
          required: ['source_update_set_id', 'new_name']
        }
      },
      // Incident convenience tools
      {
        name: 'SN-Add-Comment',
        description: 'Add a comment to an incident. Accepts incident number for better UX.',
        inputSchema: {
          type: 'object',
          properties: {
            incident_number: {
              type: 'string',
              description: 'Incident number (e.g., "INC0012345") (required)'
            },
            comment: {
              type: 'string',
              description: 'Comment text to add (required)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['incident_number', 'comment']
        }
      },
      {
        name: 'SN-Add-Work-Notes',
        description: 'Add work notes to an incident. Accepts incident number for better UX.',
        inputSchema: {
          type: 'object',
          properties: {
            incident_number: {
              type: 'string',
              description: 'Incident number (e.g., "INC0012345") (required)'
            },
            work_notes: {
              type: 'string',
              description: 'Work notes text to add (required)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['incident_number', 'work_notes']
        }
      },
      {
        name: 'SN-Assign-Incident',
        description: 'Assign an incident to a user and/or group. Resolves user names automatically.',
        inputSchema: {
          type: 'object',
          properties: {
            incident_number: {
              type: 'string',
              description: 'Incident number (e.g., "INC0012345") (required)'
            },
            assigned_to: {
              type: 'string',
              description: 'User name or sys_id to assign to (required)'
            },
            assignment_group: {
              type: 'string',
              description: 'Assignment group name or sys_id (optional)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['incident_number', 'assigned_to']
        }
      },
      {
        name: 'SN-Resolve-Incident',
        description: 'Resolve an incident with resolution notes and code.',
        inputSchema: {
          type: 'object',
          properties: {
            incident_number: {
              type: 'string',
              description: 'Incident number (e.g., "INC0012345") (required)'
            },
            resolution_notes: {
              type: 'string',
              description: 'Resolution notes describing the fix (required)'
            },
            resolution_code: {
              type: 'string',
              description: 'Resolution code (e.g., "Solved (Permanently)", "Solved (Work Around)") (optional)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['incident_number', 'resolution_notes']
        }
      },
      {
        name: 'SN-Close-Incident',
        description: 'Close an incident with close notes and code.',
        inputSchema: {
          type: 'object',
          properties: {
            incident_number: {
              type: 'string',
              description: 'Incident number (e.g., "INC0012345") (required)'
            },
            close_notes: {
              type: 'string',
              description: 'Close notes (required)'
            },
            close_code: {
              type: 'string',
              description: 'Close code (e.g., "Solved (Permanently)", "Solved (Work Around)") (optional)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['incident_number', 'close_notes']
        }
      },
      // Change Request convenience tools
      {
        name: 'SN-Add-Change-Comment',
        description: 'Add a comment to a change request. Accepts change number for better UX.',
        inputSchema: {
          type: 'object',
          properties: {
            change_number: {
              type: 'string',
              description: 'Change request number (e.g., "CHG0012345") (required)'
            },
            comment: {
              type: 'string',
              description: 'Comment text to add (required)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['change_number', 'comment']
        }
      },
      {
        name: 'SN-Assign-Change',
        description: 'Assign a change request to a user and/or group.',
        inputSchema: {
          type: 'object',
          properties: {
            change_number: {
              type: 'string',
              description: 'Change request number (e.g., "CHG0012345") (required)'
            },
            assigned_to: {
              type: 'string',
              description: 'User name or sys_id to assign to (required)'
            },
            assignment_group: {
              type: 'string',
              description: 'Assignment group name or sys_id (optional)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['change_number', 'assigned_to']
        }
      },
      {
        name: 'SN-Approve-Change',
        description: 'Approve a change request.',
        inputSchema: {
          type: 'object',
          properties: {
            change_number: {
              type: 'string',
              description: 'Change request number (e.g., "CHG0012345") (required)'
            },
            approval_comments: {
              type: 'string',
              description: 'Comments for the approval (optional)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['change_number']
        }
      },
      // Problem convenience tools
      {
        name: 'SN-Add-Problem-Comment',
        description: 'Add a comment to a problem. Accepts problem number for better UX.',
        inputSchema: {
          type: 'object',
          properties: {
            problem_number: {
              type: 'string',
              description: 'Problem number (e.g., "PRB0012345") (required)'
            },
            comment: {
              type: 'string',
              description: 'Comment text to add (required)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['problem_number', 'comment']
        }
      },
      {
        name: 'SN-Close-Problem',
        description: 'Close a problem with resolution information.',
        inputSchema: {
          type: 'object',
          properties: {
            problem_number: {
              type: 'string',
              description: 'Problem number (e.g., "PRB0012345") (required)'
            },
            resolution_notes: {
              type: 'string',
              description: 'Resolution notes (required)'
            },
            resolution_code: {
              type: 'string',
              description: 'Resolution code (optional)'
            },
            instance: {
              type: 'string',
              description: 'Instance name (optional, uses default if not specified)'
            }
          },
          required: ['problem_number', 'resolution_notes']
        }
      }
    ];

    console.error(`✅ Returning ${tools.length} consolidated tools to Claude Code`);
    return { tools };
  });

  server.setRequestHandler(CallToolRequestSchema, async (request) => {
    const { name, arguments: args } = request.params;

    try {
      switch (name) {
        case 'SN-Set-Instance': {
          const { instance_name } = args;

          // If no instance name provided, list available instances
          if (!instance_name) {
            const instances = configManager.listInstances();
            return {
              content: [{
                type: 'text',
                text: JSON.stringify({
                  message: 'Available ServiceNow instances',
                  current_instance: serviceNowClient.getCurrentInstance(),
                  instances: instances
                }, null, 2)
              }]
            };
          }

          // Get instance configuration
          const instance = configManager.getInstance(instance_name);

          // Switch the client to the new instance
          serviceNowClient.setInstance(instance.url, instance.username, instance.password, instance.name, {
            authType: instance.authType || 'basic',
            clientId: instance.clientId,
            clientSecret: instance.clientSecret,
            grantType: instance.grantType,
            scope: instance.scope
          });

          console.error(`🔄 Switched to instance: ${instance.name} (${instance.url})`);

          return {
            content: [{
              type: 'text',
              text: JSON.stringify({
                success: true,
                message: `Switched to ServiceNow instance: ${instance.name}`,
                instance: {
                  name: instance.name,
                  url: instance.url,
                  description: instance.description
                }
              }, null, 2)
            }]
          };
        }

        case 'SN-Get-Current-Instance': {
          const currentInstance = serviceNowClient.getCurrentInstance();
          return {
            content: [{
              type: 'text',
              text: JSON.stringify({
                current_instance: currentInstance,
                message: `Currently connected to: ${currentInstance.name} (${currentInstance.url})`
              }, null, 2)
            }]
          };
        }

        case 'SN-Query-Table': {
          const { table_name, query, fields, limit = 25, offset, order_by } = args;

          const queryParams = {
            sysparm_limit: limit,
            sysparm_query: query,
            sysparm_fields: fields,
            sysparm_offset: offset
          };

          if (order_by) {
            queryParams.sysparm_order_by = order_by;
          }

          const results = await serviceNowClient.getRecords(table_name, queryParams);

          return {
            content: [{
              type: 'text',
              text: `Found ${results.length} records in ${table_name}:\n${JSON.stringify(results, null, 2)}`
            }]
          };
        }

        case 'SN-Create-Record': {
          const { table_name, data } = args;
          const result = await serviceNowClient.createRecord(table_name, data);

          const metadata = tableMetadata[table_name];
          const keyField = metadata?.key_field || 'sys_id';
          const identifier = result[keyField] || result.sys_id;

          return {
            content: [{
              type: 'text',
              text: `Created ${metadata?.label || table_name} successfully: ${identifier}\n${JSON.stringify(result, null, 2)}`
            }]
          };
        }

        case 'SN-Get-Record': {
          const { table_name, sys_id, fields } = args;

          const queryParams = {};
          if (fields) queryParams.sysparm_fields = fields;

          const result = await serviceNowClient.getRecord(table_name, sys_id, queryParams);

          return {
            content: [{
              type: 'text',
              text: JSON.stringify(result, null, 2)
            }]
          };
        }

        case 'SN-Update-Record': {
          const { table_name, sys_id, data } = args;
          const result = await serviceNowClient.updateRecord(table_name, sys_id, data);

          const metadata = tableMetadata[table_name];
          const keyField = metadata?.key_field || 'sys_id';
          const identifier = result[keyField] || sys_id;

          return {
            content: [{
              type: 'text',
              text: `Updated ${metadata?.label || table_name} ${identifier} successfully\n${JSON.stringify(result, null, 2)}`
            }]
          };
        }

        case 'SN-Get-Table-Schema': {
          const { table_name } = args;
          const schema = tableMetadata[table_name];

          if (!schema) {
            // FALLBACK: Try to fetch schema from ServiceNow API
            console.error(`⚠️  Table "${table_name}" not in local metadata, attempting API fallback...`);
            try {
              const apiSchema = await serviceNowClient.discoverTableSchema(table_name, {
                include_type_codes: false,
                include_choice_tables: false,
                include_relationships: false
              });

              return {
                content: [{
                  type: 'text',
                  text: `Schema for ${table_name} (fetched from ServiceNow API):\n${JSON.stringify({
                    table_name,
                    label: apiSchema.label,
                    fields: apiSchema.fields,
                    source: 'live_api',
                    note: 'This table is not in local metadata. Consider adding it to comprehensive-table-definitions.json for faster lookups.'
                  }, null, 2)}`
                }]
              };
            } catch (error) {
              return {
                content: [{
                  type: 'text',
                  text: `No schema metadata found for table "${table_name}" in local cache, and API lookup failed: ${error.message}. The table may not exist or you may not have permissions. Use SN-Query-Table to attempt to query it.`
                }],
                isError: false
              };
            }
          }

          return {
            content: [{
              type: 'text',
              text: JSON.stringify({
                table_name,
                label: schema.label,
                key_field: schema.key_field,
                display_field: schema.display_field,
                required_fields: schema.required_fields || [],
                common_fields: schema.common_fields || [],
                operations: schema.operations || ['create', 'read', 'update', 'list'],
                description: schema.description,
                package: schema.package,
                source: 'local_cache'
              }, null, 2)
            }]
          };
        }

        case 'SN-List-Available-Tables': {
          const { category } = args;

          const categories = {
            core_itsm: ['incident', 'change_request', 'problem', 'change_task', 'problem_task'],
            platform: ['sys_user', 'sys_user_group', 'sys_db_object', 'sys_dictionary', 'sys_properties'],
            service_catalog: ['sc_request', 'sc_req_item', 'sc_cat_item', 'sc_category'],
            cmdb: ['cmdb_ci', 'cmdb_ci_computer', 'cmdb_ci_server', 'cmdb_rel_ci']
          };

          let tablesToList = Object.keys(tableMetadata);

          if (category && category !== 'all' && categories[category]) {
            tablesToList = tablesToList.filter(t => categories[category].includes(t));
          }

          const tableList = tablesToList.map(tableName => {
            const meta = tableMetadata[tableName];
            return {
              table_name: tableName,
              label: meta.label,
              description: meta.description,
              key_field: meta.key_field,
              priority: meta.priority,
              package: meta.package
            };
          });

          return {
            content: [{
              type: 'text',
              text: `Available ServiceNow tables (${tableList.length} total):\n${JSON.stringify(tableList, null, 2)}`
            }]
          };
        }

        // Convenience tool handlers
        case 'SN-List-Incidents': {
          const { state, priority, query, limit = 25, offset, fields, order_by } = args;

          let finalQuery = query || '';
          if (state && !finalQuery.includes('state')) {
            finalQuery += (finalQuery ? '^' : '') + `state=${state}`;
          }
          if (priority && !finalQuery.includes('priority')) {
            finalQuery += (finalQuery ? '^' : '') + `priority=${priority}`;
          }

          const queryParams = {
            sysparm_limit: limit,
            sysparm_query: finalQuery || undefined,
            sysparm_fields: fields,
            sysparm_offset: offset
          };

          if (order_by) {
            queryParams.sysparm_order_by = order_by;
          }

          const results = await serviceNowClient.getRecords('incident', queryParams);

          return {
            content: [{
              type: 'text',
              text: `Found ${results.length} Incident(s):\n${JSON.stringify(results, null, 2)}`
            }]
          };
        }

        case 'SN-Create-Incident': {
          const result = await serviceNowClient.createRecord('incident', args);
          return {
            content: [{
              type: 'text',
              text: `Created Incident: ${result.number}\n${JSON.stringify(result, null, 2)}`
            }]
          };
        }

        case 'SN-Get-Incident': {
          const result = await serviceNowClient.getRecord('incident', args.sys_id);
          return {
            content: [{
              type: 'text',
              text: JSON.stringify(result, null, 2)
            }]
          };
        }

        case 'SN-List-SysUsers': {
          const { query, limit = 25, offset, fields, order_by } = args;

          const queryParams = {
            sysparm_limit: limit,
            sysparm_query: query,
            sysparm_fields: fields,
            sysparm_offset: offset
          };

          if (order_by) {
            queryParams.sysparm_order_by = order_by;
          }

          const results = await serviceNowClient.getRecords('sys_user', queryParams);

          return {
            content: [{
              type: 'text',
              text: `Found ${results.length} Sys User(s):\n${JSON.stringify(results, null, 2)}`
            }]
          };
        }

        case 'SN-List-CmdbCis': {
          const { query, limit = 25, offset, fields, order_by } = args;

          const queryParams = {
            sysparm_limit: limit,
            sysparm_query: query,
            sysparm_fields: fields,
            sysparm_offset: offset
          };

          if (order_by) {
            queryParams.sysparm_order_by = order_by;
          }

          const results = await serviceNowClient.getRecords('cmdb_ci', queryParams);

          return {
            content: [{
              type: 'text',
              text: `Found ${results.length} Cmdb Ci(s):\n${JSON.stringify(results, null, 2)}`
            }]
          };
        }

        case 'SN-List-SysUserGroups': {
          const { query, limit = 25, offset, fields, order_by } = args;

          const queryParams = {
            sysparm_limit: limit,
            sysparm_query: query,
            sysparm_fields: fields,
            sysparm_offset: offset
          };

          if (order_by) {
            queryParams.sysparm_order_by = order_by;
          }

          const results = await serviceNowClient.getRecords('sys_user_group', queryParams);

          return {
            content: [{
              type: 'text',
              text: `Found ${results.length} Sys User Group(s):\n${JSON.stringify(results, null, 2)}`
            }]
          };
        }

        case 'SN-List-ChangeRequests': {
          const { state, type, query, limit = 25, offset, fields, order_by } = args;

          let finalQuery = query || '';
          if (state && !finalQuery.includes('state')) {
            finalQuery += (finalQuery ? '^' : '') + `state=${state}`;
          }
          if (type && !finalQuery.includes('type')) {
            finalQuery += (finalQuery ? '^' : '') + `type=${type}`;
          }

          const queryParams = {
            sysparm_limit: limit,
            sysparm_query: finalQuery || undefined,
            sysparm_fields: fields,
            sysparm_offset: offset
          };

          if (order_by) {
            queryParams.sysparm_order_by = order_by;
          }

          const results = await serviceNowClient.getRecords('change_request', queryParams);

          return {
            content: [{
              type: 'text',
              text: `Found ${results.length} Change Request(s):\n${JSON.stringify(results, null, 2)}`
            }]
          };
        }

        case 'SN-List-Problems': {
          const { query, limit = 25, offset, fields, order_by } = args;

          const queryParams = {
            sysparm_limit: limit,
            sysparm_query: query,
            sysparm_fields: fields,
            sysparm_offset: offset
          };

          if (order_by) {
            queryParams.sysparm_order_by = order_by;
          }

          const results = await serviceNowClient.getRecords('problem', queryParams);

          return {
            content: [{
              type: 'text',
              text: `Found ${results.length} Problem(s):\n${JSON.stringify(results, null, 2)}`
            }]
          };
        }

        case 'SN-Natural-Language-Search': {
          const { query, table = 'incident', limit = 25, fields, order_by, show_patterns = true } = args;

          console.error(`🔍 Natural language search: "${query}" on ${table}`);

          // Parse natural language query
          const parseResult = parseNaturalLanguage(query, table);

          // Check if parsing succeeded
          if (!parseResult.encodedQuery) {
            return {
              content: [{
                type: 'text',
                text: `❌ Unable to parse query: "${query}"

${parseResult.suggestions.join('\n')}

Unmatched text: "${parseResult.unmatchedText}"

${show_patterns ? `\n## Supported Patterns:\n${JSON.stringify(getSupportedPatterns(), null, 2)}` : ''}`
              }]
            };
          }

          // Execute the encoded query
          const queryParams = {
            sysparm_limit: limit,
            sysparm_query: parseResult.encodedQuery,
            sysparm_fields: fields,
            sysparm_offset: 0
          };

          if (order_by) {
            queryParams.sysparm_order_by = order_by;
          }

          const results = await serviceNowClient.getRecords(table, queryParams);

          // Build response
          let responseText = `✅ Natural Language Search Results

**Original Query:** "${query}"
**Target Table:** ${table}
**Parsed Encoded Query:** \`${parseResult.encodedQuery}\`
**Records Found:** ${results.length}/${limit}

`;

          // Add pattern matching details if requested
          if (show_patterns && parseResult.matchedPatterns.length > 0) {
            responseText += `## Matched Patterns:\n`;
            parseResult.matchedPatterns.forEach((p, idx) => {
              responseText += `${idx + 1}. **"${p.matched}"** → \`${p.condition}\`\n`;
            });
            responseText += `\n`;
          }

          // Add warnings for unmatched text
          if (parseResult.unmatchedText && parseResult.unmatchedText.length > 3) {
            responseText += `⚠️ **Unrecognized:** "${parseResult.unmatchedText}"\n\n`;
          }

          // Add results
          if (results.length > 0) {
            responseText += `## Results:\n\`\`\`json\n${JSON.stringify(results, null, 2)}\n\`\`\``;
          } else {
            responseText += `## No records found matching the query.\n\nTry adjusting your search criteria or use SN-Query-Table for more control.`;
          }

          return {
            content: [{
              type: 'text',
              text: responseText
            }]
          };
        }

        case 'SN-Set-Update-Set': {
          const { update_set_sys_id } = args;

          console.error(`🔄 Setting current update set to: ${update_set_sys_id}`);

          try {
            // Try to set via API (UI endpoint or sys_trigger)
            const result = await serviceNowClient.setCurrentUpdateSet(update_set_sys_id);

            if (result.method === 'sys_trigger') {
              return {
                content: [{
                  type: 'text',
                  text: `✅ Update set change scheduled via sys_trigger!

Update Set: ${result.update_set}
sys_id: ${result.sys_id}

🔧 Method: sys_trigger (scheduled job)
📊 Trigger Details:
- Trigger sys_id: ${result.trigger_details.trigger_sys_id}
- Trigger name: ${result.trigger_details.trigger_name}
- Scheduled time: ${result.trigger_details.next_action}
- Auto-delete: ${result.trigger_details.auto_delete ? 'Yes' : 'No'}

The script will execute in ~1 second and set your current update set. Refresh your ServiceNow browser after 2 seconds to see the change in the top bar.`
                }]
              };
            } else {
              return {
                content: [{
                  type: 'text',
                  text: `✅ Update set set to current: ${result.update_set}

🔧 Method: UI API endpoint (/api/now/ui/concoursepicker/updateset)
📊 Response: ${JSON.stringify(result.response, null, 2)}

The update set has been set as your current update set. Refresh your ServiceNow browser to see the change in the top bar.`
                }]
              };
            }
          } catch (error) {
            // If both methods fail, fall back to creating fix script
            console.error('⚠️  Direct update set change failed, creating fix script...');

            const updateSet = await serviceNowClient.getRecord('sys_update_set', update_set_sys_id);

            const fs = await import('fs/promises');
            const path = await import('path');

            const scriptsDir = path.resolve(process.cwd(), 'scripts');
            await fs.mkdir(scriptsDir, { recursive: true });

            const timestamp = new Date().toISOString().replace(/[:.]/g, '-');
            const fileName = `set_update_set_${updateSet.name?.replace(/[^a-zA-Z0-9]/g, '_')}_${timestamp}.js`;
            const filePath = path.join(scriptsDir, fileName);

            const scriptContent = `// Set current update set using GlideUpdateSet API
var gus = new GlideUpdateSet();
gus.set('${update_set_sys_id}');
gs.info('✅ Update set changed to: ${updateSet.name}');`;

            const fileContent = `/**
 * Fix Script: Set Current Update Set
 * Update Set: ${updateSet.name}
 * Update Set sys_id: ${update_set_sys_id}
 * Created: ${new Date().toISOString()}
 *
 * Note: Automated methods failed. Manual execution required.
 *
 * INSTRUCTIONS:
 * 1. Copy the script below (the GlideUpdateSet part)
 * 2. Navigate to ServiceNow: System Definition → Scripts - Background
 * 3. Paste the script
 * 4. Click "Run script"
 * 5. Verify output: "Update set changed to: ${updateSet.name}"
 * 6. Refresh your browser to see the update set in the top bar
 *
 * ALTERNATIVE: Manual UI Method
 * 1. Navigate to: System Update Sets → Local Update Sets
 * 2. Find: ${updateSet.name}
 * 3. Click "Make this my current set"
 */

${scriptContent}`;

            await fs.writeFile(filePath, fileContent, 'utf-8');

            return {
              content: [{
                type: 'text',
                text: `⚠️ Automated update set change not available.
Created fix script for manual execution: ${filePath}

Update Set: ${updateSet.name}
Sys ID: ${update_set_sys_id}

🔧 To Apply:
1. Open: ${filePath}
2. Copy the GlideUpdateSet script
3. Run in ServiceNow: System Definition → Scripts - Background
4. Refresh browser to see change

💡 Alternative: Set manually in UI (System Update Sets → Local Update Sets → Make Current)`
              }]
            };
          }
        }

        case 'SN-Get-Current-Update-Set': {
          const result = await serviceNowClient.getCurrentUpdateSet();

          return {
            content: [{
              type: 'text',
              text: `Current update set:\n${JSON.stringify(result, null, 2)}`
            }]
          };
        }

        case 'SN-List-Update-Sets': {
          const { query, limit = 25, offset, fields, order_by } = args;

          const queryParams = {
            sysparm_limit: limit,
            sysparm_query: query,
            sysparm_fields: fields,
            sysparm_offset: offset
          };

          if (order_by) {
            queryParams.sysparm_order_by = order_by;
          }

          const results = await serviceNowClient.listUpdateSets(queryParams);

          return {
            content: [{
              type: 'text',
              text: `Found ${results.length} Update Set(s):\n${JSON.stringify(results, null, 2)}`
            }]
          };
        }

        case 'SN-Set-Current-Application': {
          const { app_sys_id } = args;

          console.error(`🔄 Setting current application to: ${app_sys_id}`);

          try {
            const result = await serviceNowClient.setCurrentApplication(app_sys_id);

            return {
              content: [{
                type: 'text',
                text: `✅ Application set to current: ${result.application}

🔧 Method: UI API endpoint (/api/now/ui/concoursepicker/application)
📊 Response: ${JSON.stringify(result.response, null, 2)}

The application scope has been set as your current application. Refresh your ServiceNow browser to see the change in the top bar.`
              }]
            };
          } catch (error) {
            console.error('❌ Failed to set current application:', error);
            return {
              content: [{
                type: 'text',
                text: `❌ Failed to set current application: ${error.message}

Please verify:
1. The app_sys_id is valid
2. You have permissions to access the application
3. The application exists in your instance`
              }]
            };
          }
        }

        case 'SN-Execute-Background-Script': {
          const { script, description } = args;

          console.error(`🚀 Executing background script via sys_trigger...`);

          try {
            // Primary method: sys_trigger (ONLY working method)
            const result = await serviceNowClient.executeScriptViaTrigger(script, description, true);

            return {
              content: [{
                type: 'text',
                text: `✅ Script scheduled for execution via sys_trigger!

${description ? `Description: ${description}\n` : ''}
📊 Trigger Details:
- Trigger sys_id: ${result.trigger_sys_id}
- Trigger name: ${result.trigger_name}
- Scheduled time: ${result.next_action}
- Auto-delete: ${result.auto_delete ? 'Yes' : 'No'}

${result.message}

The script will execute in ~1 second. You can monitor execution in:
- System Logs → System Log → All
- System Definition → Scheduled Jobs (filter by name: ${result.trigger_name})

🔍 Script to execute:
${script.substring(0, 300)}${script.length > 300 ? '...' : ''}`
              }]
            };
          } catch (triggerError) {
            // Fallback: Create fix script if sys_trigger fails
            console.error('⚠️  Trigger method failed, creating fix script...', triggerError.message);

            // Fallback: Create fix script file
            const fs = await import('fs/promises');
            const path = await import('path');
            const timestamp = new Date().toISOString().replace(/[:.]/g, '-');
            const script_name = `background_script_${timestamp}`;

            const scriptsDir = path.resolve(process.cwd(), 'scripts');
            await fs.mkdir(scriptsDir, { recursive: true });

            const fileName = `${script_name}.js`;
            const filePath = path.join(scriptsDir, fileName);

            const fileContent = `/**
 * Background Script (Manual Execution Required)
 * Created: ${new Date().toISOString()}
 * ${description ? `Description: ${description}` : ''}
 *
 * Note: Direct execution failed due to authentication requirements.
 * This script must be executed manually in ServiceNow UI.
 *
 * INSTRUCTIONS:
 * 1. Copy the script below
 * 2. Navigate to ServiceNow: System Definition → Scripts - Background
 * 3. Paste the script
 * 4. Click "Run script"
 * 5. Verify output in the output panel
 */

${script}

// End of script
`;

            await fs.writeFile(filePath, fileContent, 'utf-8');

            return {
              content: [{
                type: 'text',
                text: `⚠️ Direct execution not available (requires UI session).
Created fix script for manual execution: ${filePath}

📋 To Execute Manually:
1. Open: ${filePath}
2. Copy the script content
3. In ServiceNow: System Definition → Scripts - Background
4. Paste and click "Run script"

Script Preview:
${script.substring(0, 200)}${script.length > 200 ? '...' : ''}`
              }]
            };
          }
        }

        case 'SN-Create-Fix-Script': {
          const { script_name, script_content, description, auto_delete = false } = args;

          console.error(`📝 Creating fix script: ${script_name}`);

          // Import fs for file operations
          const fs = await import('fs/promises');
          const path = await import('path');

          // Ensure /scripts directory exists
          const scriptsDir = path.resolve(process.cwd(), 'scripts');
          await fs.mkdir(scriptsDir, { recursive: true });

          // Generate script file with header
          const timestamp = new Date().toISOString().replace(/[:.]/g, '-');
          const fileName = `${script_name}_${timestamp}.js`;
          const filePath = path.join(scriptsDir, fileName);

          const fileContent = `/**
 * Fix Script: ${script_name}
 * Created: ${new Date().toISOString()}
 * ${description ? `Description: ${description}` : ''}
 *
 * INSTRUCTIONS:
 * 1. Copy the entire script below
 * 2. Navigate to ServiceNow: System Definition → Scripts - Background
 * 3. Paste the script
 * 4. Click "Run script"
 * 5. Verify output in the output panel
 * ${auto_delete ? '6. Delete this file after successful execution' : ''}
 */

${script_content}

// End of script
`;

          await fs.writeFile(filePath, fileContent, 'utf-8');

          return {
            content: [{
              type: 'text',
              text: `✅ Fix script created: ${filePath}

📋 Next Steps:
1. Open the file: ${filePath}
2. Copy the entire script content
3. In ServiceNow, navigate to: System Definition → Scripts - Background
4. Paste and run the script
5. Verify the output${auto_delete ? '\n6. Delete the file after successful execution' : ''}

Script Preview (first 200 chars):
${script_content.substring(0, 200)}${script_content.length > 200 ? '...' : ''}`
            }]
          };
        }

        case 'SN-Discover-Table-Schema': {
          const {
            table_name,
            include_type_codes = false,
            include_choice_tables = false,
            include_relationships = false,
            include_ui_policies = false,
            include_business_rules = false,
            include_field_constraints = false
          } = args;

          console.error(`🔍 Discovering enhanced schema for ${table_name}`);
          const schema = await serviceNowClient.discoverTableSchema(table_name, {
            include_type_codes,
            include_choice_tables,
            include_relationships,
            include_ui_policies,
            include_business_rules,
            include_field_constraints
          });

          return {
            content: [{
              type: 'text',
              text: `Enhanced schema for ${table_name}:\n${JSON.stringify(schema, null, 2)}`
            }]
          };
        }

        case 'SN-Batch-Create': {
          const { operations, transaction = true, progress = true } = args;

          console.error(`📦 Batch creating ${operations.length} records (transaction: ${transaction}, progress: ${progress})`);
          const result = await serviceNowClient.batchCreate(operations, transaction, progress);

          return {
            content: [{
              type: 'text',
              text: `Batch create ${result.success ? 'completed' : 'failed'}:\n${JSON.stringify(result, null, 2)}`
            }]
          };
        }

        case 'SN-Batch-Update': {
          const { updates, stop_on_error = false, progress = true } = args;

          console.error(`📦 Batch updating ${updates.length} records (progress: ${progress})`);
          const result = await serviceNowClient.batchUpdate(updates, stop_on_error, progress);

          return {
            content: [{
              type: 'text',
              text: `Batch update ${result.success ? 'completed' : 'completed with errors'}:\n${JSON.stringify(result, null, 2)}`
            }]
          };
        }

        case 'SN-Explain-Field': {
          const { table, field, include_examples = true } = args;

          console.error(`📖 Explaining field ${table}.${field}`);
          const explanation = await serviceNowClient.explainField(table, field, include_examples);

          return {
            content: [{
              type: 'text',
              text: `Field explanation for ${table}.${field}:\n${JSON.stringify(explanation, null, 2)}`
            }]
          };
        }

        case 'SN-Validate-Configuration': {
          const { catalog_item, checks = {} } = args;

          console.error(`✅ Validating catalog item ${catalog_item}`);
          const validation = await serviceNowClient.validateCatalogConfiguration(catalog_item, checks);

          return {
            content: [{
              type: 'text',
              text: `Validation ${validation.valid ? 'PASSED' : 'FAILED'}:\n${JSON.stringify(validation, null, 2)}`
            }]
          };
        }

        case 'SN-Inspect-Update-Set': {
          const { update_set, show_components = true, show_dependencies = false } = args;

          console.error(`🔎 Inspecting update set ${update_set}`);
          const inspection = await serviceNowClient.inspectUpdateSet(update_set, {
            show_components,
            show_dependencies
          });

          return {
            content: [{
              type: 'text',
              text: `Update set inspection:\n${JSON.stringify(inspection, null, 2)}`
            }]
          };
        }

        case 'SN-Create-Workflow': {
          const { name, description, table, condition, activities, transitions, publish = false, progress = true } = args;

          console.error(`🔄 Creating workflow: ${name} (progress: ${progress})`);

          // Build workflow specification
          const workflowSpec = {
            name,
            description,
            table,
            condition,
            activities,
            transitions,
            publish
          };

          const result = await serviceNowClient.createCompleteWorkflow(workflowSpec, progress);

          return {
            content: [{
              type: 'text',
              text: `✅ Workflow created successfully!

Workflow: ${result.workflow_name}
Workflow sys_id: ${result.workflow_sys_id}
Version sys_id: ${result.version_sys_id}
Status: ${result.published ? 'Published' : 'Draft'}

Activities created: ${result.activities.length}
${result.activities.map(a => `  - ${a.name} (${a.activity_sys_id})`).join('\n')}

Transitions created: ${result.transitions.length}

${result.published ? '✅ Workflow is published and ready to use!' : '⚠️ Workflow is in draft mode. Use SN-Publish-Workflow to publish it.'}

Full result:
${JSON.stringify(result, null, 2)}`
            }]
          };
        }

        case 'SN-Create-Activity': {
          const { workflow_version_sys_id, name, script, activity_definition_sys_id, x, y } = args;

          console.error(`➕ Creating activity: ${name}`);

          const activityData = {
            workflow_version_sys_id,
            name,
            script,
            activity_definition_sys_id,
            x,
            y
          };

          const result = await serviceNowClient.createActivity(activityData);

          return {
            content: [{
              type: 'text',
              text: `✅ Activity created successfully!

Activity: ${result.name}
Activity sys_id: ${result.activity_sys_id}

You can now:
- Create transitions to/from this activity using SN-Create-Transition
- Add this activity to workflow canvas in ServiceNow UI`
            }]
          };
        }

        case 'SN-Create-Transition': {
          const { from_activity_sys_id, to_activity_sys_id, condition_script, order } = args;

          console.error(`🔗 Creating transition`);

          const transitionData = {
            from_activity_sys_id,
            to_activity_sys_id,
            order
          };

          // If condition script provided, create condition first
          let condition_sys_id = null;
          if (condition_script) {
            const conditionData = {
              activity_sys_id: from_activity_sys_id,
              name: 'Transition Condition',
              condition: condition_script
            };
            const conditionResult = await serviceNowClient.createCondition(conditionData);
            condition_sys_id = conditionResult.condition_sys_id;
            transitionData.condition_sys_id = condition_sys_id;
          }

          const result = await serviceNowClient.createTransition(transitionData);

          return {
            content: [{
              type: 'text',
              text: `✅ Transition created successfully!

Transition sys_id: ${result.transition_sys_id}
From activity: ${from_activity_sys_id}
To activity: ${to_activity_sys_id}
${condition_sys_id ? `Condition sys_id: ${condition_sys_id}` : 'No condition (always transitions)'}

The workflow will now transition from the source activity to the target activity${condition_script ? ' when the condition is met' : ''}.`
            }]
          };
        }

        case 'SN-Publish-Workflow': {
          const { version_sys_id, start_activity_sys_id } = args;

          console.error(`🚀 Publishing workflow version ${version_sys_id}`);

          const result = await serviceNowClient.publishWorkflow(version_sys_id, start_activity_sys_id);

          return {
            content: [{
              type: 'text',
              text: `✅ Workflow published successfully!

Version sys_id: ${result.version_sys_id}
Start activity: ${result.start_activity}
Status: Published

The workflow is now active and will trigger based on its configured conditions.`
            }]
          };
        }

        case 'SN-Move-Records-To-Update-Set': {
          const { update_set_id, record_sys_ids, time_range, source_update_set, table, progress = true } = args;

          console.error(`📦 Moving records to update set ${update_set_id} (progress: ${progress})`);

          const result = await serviceNowClient.moveRecordsToUpdateSet(update_set_id, {
            record_sys_ids,
            time_range,
            source_update_set,
            table,
            reportProgress: progress
          });

          return {
            content: [{
              type: 'text',
              text: `✅ Records moved to update set!

Moved: ${result.moved} records
Failed: ${result.failed} records

${result.records.length > 0 ? `\nMoved records:\n${result.records.map(r => `  - ${r.type}: ${r.name} (${r.sys_id})`).join('\n')}` : ''}

${result.errors.length > 0 ? `\n❌ Errors:\n${result.errors.map(e => `  - ${e.sys_id}: ${e.error}`).join('\n')}` : ''}

Full result:
${JSON.stringify(result, null, 2)}`
            }]
          };
        }

        case 'SN-Clone-Update-Set': {
          const { source_update_set_id, new_name, progress = true } = args;

          console.error(`🔄 Cloning update set ${source_update_set_id} (progress: ${progress})`);

          const result = await serviceNowClient.cloneUpdateSet(source_update_set_id, new_name, progress);

          return {
            content: [{
              type: 'text',
              text: `✅ Update set cloned successfully!

Source Update Set: ${result.source_update_set_name}
Source sys_id: ${result.source_update_set_id}

New Update Set: ${result.new_update_set_name}
New sys_id: ${result.new_update_set_id}

Records cloned: ${result.records_cloned} / ${result.total_source_records}

The cloned update set is now in "In Progress" state and ready for use.`
            }]
          };
        }

        // Incident convenience tool handlers
        case 'SN-Add-Comment': {
          const { incident_number, comment } = args;

          // Look up incident by number
          const incidents = await serviceNowClient.getRecords('incident', {
            sysparm_query: `number=${incident_number}`,
            sysparm_limit: 1
          });

          if (!incidents || incidents.length === 0) {
            throw new Error(`Incident ${incident_number} not found`);
          }

          const incident = incidents[0];

          // Update comments field
          const result = await serviceNowClient.updateRecord('incident', incident.sys_id, {
            comments: comment
          });

          return {
            content: [{
              type: 'text',
              text: `✅ Comment added to ${incident_number}

Incident: ${incident_number}
sys_id: ${incident.sys_id}
Comment: ${comment}
Updated: ${new Date().toISOString()}

The comment has been successfully added to the incident.`
            }]
          };
        }

        case 'SN-Add-Work-Notes': {
          const { incident_number, work_notes } = args;

          // Look up incident by number
          const incidents = await serviceNowClient.getRecords('incident', {
            sysparm_query: `number=${incident_number}`,
            sysparm_limit: 1
          });

          if (!incidents || incidents.length === 0) {
            throw new Error(`Incident ${incident_number} not found`);
          }

          const incident = incidents[0];

          // Update work_notes field
          const result = await serviceNowClient.updateRecord('incident', incident.sys_id, {
            work_notes: work_notes
          });

          return {
            content: [{
              type: 'text',
              text: `✅ Work notes added to ${incident_number}

Incident: ${incident_number}
sys_id: ${incident.sys_id}
Work Notes: ${work_notes}
Updated: ${new Date().toISOString()}

The work notes have been successfully added to the incident.`
            }]
          };
        }

        case 'SN-Assign-Incident': {
          const { incident_number, assigned_to, assignment_group } = args;

          // Look up incident by number
          const incidents = await serviceNowClient.getRecords('incident', {
            sysparm_query: `number=${incident_number}`,
            sysparm_limit: 1
          });

          if (!incidents || incidents.length === 0) {
            throw new Error(`Incident ${incident_number} not found`);
          }

          const incident = incidents[0];

          // Resolve user if not a sys_id (32 character hex string)
          let assignedToId = assigned_to;
          if (!/^[0-9a-f]{32}$/i.test(assigned_to)) {
            const users = await serviceNowClient.getRecords('sys_user', {
              sysparm_query: `name=${assigned_to}^ORuser_name=${assigned_to}`,
              sysparm_limit: 1
            });

            if (!users || users.length === 0) {
              throw new Error(`User "${assigned_to}" not found`);
            }

            assignedToId = users[0].sys_id;
          }

          // Resolve group if provided and not a sys_id
          let assignmentGroupId = assignment_group;
          if (assignment_group && !/^[0-9a-f]{32}$/i.test(assignment_group)) {
            const groups = await serviceNowClient.getRecords('sys_user_group', {
              sysparm_query: `name=${assignment_group}`,
              sysparm_limit: 1
            });

            if (!groups || groups.length === 0) {
              throw new Error(`Group "${assignment_group}" not found`);
            }

            assignmentGroupId = groups[0].sys_id;
          }

          // Update assignment fields
          const updateData = {
            assigned_to: assignedToId
          };

          if (assignmentGroupId) {
            updateData.assignment_group = assignmentGroupId;
          }

          const result = await serviceNowClient.updateRecord('incident', incident.sys_id, updateData);

          return {
            content: [{
              type: 'text',
              text: `✅ ${incident_number} assigned successfully

Incident: ${incident_number}
sys_id: ${incident.sys_id}
Assigned To: ${result.assigned_to?.display_value || assignedToId}
${assignmentGroupId ? `Assignment Group: ${result.assignment_group?.display_value || assignmentGroupId}` : ''}
Updated: ${new Date().toISOString()}

The incident has been assigned successfully.`
            }]
          };
        }

        case 'SN-Resolve-Incident': {
          const { incident_number, resolution_notes, resolution_code } = args;

          // Look up incident by number
          const incidents = await serviceNowClient.getRecords('incident', {
            sysparm_query: `number=${incident_number}`,
            sysparm_limit: 1
          });

          if (!incidents || incidents.length === 0) {
            throw new Error(`Incident ${incident_number} not found`);
          }

          const incident = incidents[0];

          // Update to resolved state (6)
          const updateData = {
            state: 6,
            close_notes: resolution_notes
          };

          if (resolution_code) {
            updateData.close_code = resolution_code;
          }

          const result = await serviceNowClient.updateRecord('incident', incident.sys_id, updateData);

          return {
            content: [{
              type: 'text',
              text: `✅ ${incident_number} resolved successfully

Incident: ${incident_number}
sys_id: ${incident.sys_id}
State: Resolved (6)
Resolution Notes: ${resolution_notes}
${resolution_code ? `Resolution Code: ${resolution_code}` : ''}
Updated: ${new Date().toISOString()}

The incident has been resolved successfully.`
            }]
          };
        }

        case 'SN-Close-Incident': {
          const { incident_number, close_notes, close_code } = args;

          // Look up incident by number
          const incidents = await serviceNowClient.getRecords('incident', {
            sysparm_query: `number=${incident_number}`,
            sysparm_limit: 1
          });

          if (!incidents || incidents.length === 0) {
            throw new Error(`Incident ${incident_number} not found`);
          }

          const incident = incidents[0];

          // Update to closed state (7)
          const updateData = {
            state: 7,
            close_notes: close_notes
          };

          if (close_code) {
            updateData.close_code = close_code;
          }

          const result = await serviceNowClient.updateRecord('incident', incident.sys_id, updateData);

          return {
            content: [{
              type: 'text',
              text: `✅ ${incident_number} closed successfully

Incident: ${incident_number}
sys_id: ${incident.sys_id}
State: Closed (7)
Close Notes: ${close_notes}
${close_code ? `Close Code: ${close_code}` : ''}
Updated: ${new Date().toISOString()}

The incident has been closed successfully.`
            }]
          };
        }

        // Change Request convenience tool handlers
        case 'SN-Add-Change-Comment': {
          const { change_number, comment } = args;

          // Look up change by number
          const changes = await serviceNowClient.getRecords('change_request', {
            sysparm_query: `number=${change_number}`,
            sysparm_limit: 1
          });

          if (!changes || changes.length === 0) {
            throw new Error(`Change request ${change_number} not found`);
          }

          const change = changes[0];

          // Update comments field
          const result = await serviceNowClient.updateRecord('change_request', change.sys_id, {
            comments: comment
          });

          return {
            content: [{
              type: 'text',
              text: `✅ Comment added to ${change_number}

Change Request: ${change_number}
sys_id: ${change.sys_id}
Comment: ${comment}
Updated: ${new Date().toISOString()}

The comment has been successfully added to the change request.`
            }]
          };
        }

        case 'SN-Assign-Change': {
          const { change_number, assigned_to, assignment_group } = args;

          // Look up change by number
          const changes = await serviceNowClient.getRecords('change_request', {
            sysparm_query: `number=${change_number}`,
            sysparm_limit: 1
          });

          if (!changes || changes.length === 0) {
            throw new Error(`Change request ${change_number} not found`);
          }

          const change = changes[0];

          // Resolve user if not a sys_id
          let assignedToId = assigned_to;
          if (!/^[0-9a-f]{32}$/i.test(assigned_to)) {
            const users = await serviceNowClient.getRecords('sys_user', {
              sysparm_query: `name=${assigned_to}^ORuser_name=${assigned_to}`,
              sysparm_limit: 1
            });

            if (!users || users.length === 0) {
              throw new Error(`User "${assigned_to}" not found`);
            }

            assignedToId = users[0].sys_id;
          }

          // Resolve group if provided and not a sys_id
          let assignmentGroupId = assignment_group;
          if (assignment_group && !/^[0-9a-f]{32}$/i.test(assignment_group)) {
            const groups = await serviceNowClient.getRecords('sys_user_group', {
              sysparm_query: `name=${assignment_group}`,
              sysparm_limit: 1
            });

            if (!groups || groups.length === 0) {
              throw new Error(`Group "${assignment_group}" not found`);
            }

            assignmentGroupId = groups[0].sys_id;
          }

          // Update assignment fields
          const updateData = {
            assigned_to: assignedToId
          };

          if (assignmentGroupId) {
            updateData.assignment_group = assignmentGroupId;
          }

          const result = await serviceNowClient.updateRecord('change_request', change.sys_id, updateData);

          return {
            content: [{
              type: 'text',
              text: `✅ ${change_number} assigned successfully

Change Request: ${change_number}
sys_id: ${change.sys_id}
Assigned To: ${result.assigned_to?.display_value || assignedToId}
${assignmentGroupId ? `Assignment Group: ${result.assignment_group?.display_value || assignmentGroupId}` : ''}
Updated: ${new Date().toISOString()}

The change request has been assigned successfully.`
            }]
          };
        }

        case 'SN-Approve-Change': {
          const { change_number, approval_comments } = args;

          // Look up change by number
          const changes = await serviceNowClient.getRecords('change_request', {
            sysparm_query: `number=${change_number}`,
            sysparm_limit: 1
          });

          if (!changes || changes.length === 0) {
            throw new Error(`Change request ${change_number} not found`);
          }

          const change = changes[0];

          // Update approval field and add comments
          const updateData = {
            approval: 'approved'
          };

          if (approval_comments) {
            updateData.comments = approval_comments;
          }

          const result = await serviceNowClient.updateRecord('change_request', change.sys_id, updateData);

          return {
            content: [{
              type: 'text',
              text: `✅ ${change_number} approved successfully

Change Request: ${change_number}
sys_id: ${change.sys_id}
Approval: approved
${approval_comments ? `Comments: ${approval_comments}` : ''}
Updated: ${new Date().toISOString()}

The change request has been approved successfully.`
            }]
          };
        }

        // Problem convenience tool handlers
        case 'SN-Add-Problem-Comment': {
          const { problem_number, comment } = args;

          // Look up problem by number
          const problems = await serviceNowClient.getRecords('problem', {
            sysparm_query: `number=${problem_number}`,
            sysparm_limit: 1
          });

          if (!problems || problems.length === 0) {
            throw new Error(`Problem ${problem_number} not found`);
          }

          const problem = problems[0];

          // Update comments field
          const result = await serviceNowClient.updateRecord('problem', problem.sys_id, {
            comments: comment
          });

          return {
            content: [{
              type: 'text',
              text: `✅ Comment added to ${problem_number}

Problem: ${problem_number}
sys_id: ${problem.sys_id}
Comment: ${comment}
Updated: ${new Date().toISOString()}

The comment has been successfully added to the problem.`
            }]
          };
        }

        case 'SN-Close-Problem': {
          const { problem_number, resolution_notes, resolution_code } = args;

          // Look up problem by number
          const problems = await serviceNowClient.getRecords('problem', {
            sysparm_query: `number=${problem_number}`,
            sysparm_limit: 1
          });

          if (!problems || problems.length === 0) {
            throw new Error(`Problem ${problem_number} not found`);
          }

          const problem = problems[0];

          // Update to resolved/closed state
          const updateData = {
            state: 3, // Resolved/Closed state for problem
            resolution_notes: resolution_notes
          };

          if (resolution_code) {
            updateData.resolution_code = resolution_code;
          }

          const result = await serviceNowClient.updateRecord('problem', problem.sys_id, updateData);

          return {
            content: [{
              type: 'text',
              text: `✅ ${problem_number} closed successfully

Problem: ${problem_number}
sys_id: ${problem.sys_id}
State: Resolved/Closed (3)
Resolution Notes: ${resolution_notes}
${resolution_code ? `Resolution Code: ${resolution_code}` : ''}
Updated: ${new Date().toISOString()}

The problem has been closed successfully.`
            }]
          };
        }

        default:
          throw new Error(`Unknown tool: ${name}`);
      }
    } catch (error) {
      return {
        content: [{
          type: 'text',
          text: `Error: ${error.message}`
        }],
        isError: true
      };
    }
  });

  // Add resources
  server.setRequestHandler(ListResourcesRequestSchema, async () => {
    return {
      resources: [
        {
          uri: 'servicenow://instance',
          mimeType: 'application/json',
          name: 'ServiceNow Instance Info',
          description: 'Information about the connected ServiceNow instance'
        },
        {
          uri: 'servicenow://tables/all',
          mimeType: 'application/json',
          name: 'All ServiceNow Tables',
          description: 'Complete list of available ServiceNow tables with metadata'
        }
      ]
    };
  });

  server.setRequestHandler(ReadResourceRequestSchema, async (request) => {
    const { uri } = request.params;

    if (uri === 'servicenow://instance') {
      const config = {
        server_info: {
          name: 'Happy MCP Server (Consolidated)',
          version: '2.0.0',
          description: 'Consolidated ServiceNow integration with metadata-driven schema lookups'
        },
        instance_info: {
          url: process.env.SERVICENOW_INSTANCE_URL,
          username: process.env.SERVICENOW_USERNAME
        },
        capabilities: {
          total_tables: Object.keys(tableMetadata).length,
          operations: ['create', 'read', 'update', 'query', 'schema_lookup'],
          tools: 6
        }
      };

      return {
        contents: [{
          uri: uri,
          mimeType: 'application/json',
          text: JSON.stringify(config, null, 2)
        }]
      };
    }

    if (uri === 'servicenow://tables/all') {
      return {
        contents: [{
          uri: uri,
          mimeType: 'application/json',
          text: JSON.stringify(tableMetadata, null, 2)
        }]
      };
    }

    throw new Error(`Unknown resource: ${uri}`);
  });

  return server;
}