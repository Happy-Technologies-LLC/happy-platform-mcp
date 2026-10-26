/**
 * Mock utilities for Happy MCP Server tests
 */

import { jest } from '@jest/globals';

/**
 * Create a mock ServiceNow client
 */
export function createMockServiceNowClient() {
  return {
    currentInstanceName: 'dev',
    instanceUrl: 'https://dev123.service-now.com',

    setInstance: jest.fn(),
    getCurrentInstance: jest.fn(() => ({
      name: 'dev',
      url: 'https://dev123.service-now.com'
    })),

    // Generic table operations
    getRecords: jest.fn(),
    getRecord: jest.fn(),
    createRecord: jest.fn(),
    updateRecord: jest.fn(),
    deleteRecord: jest.fn(),

    // Update set management
    setCurrentUpdateSet: jest.fn(),
    getCurrentUpdateSet: jest.fn(),
    listUpdateSets: jest.fn(),

    // Background script execution
    executeScriptViaProcessor: jest.fn(),
    executeScriptViaTrigger: jest.fn(),

    // Schema discovery
    discoverTableSchema: jest.fn(),

    // Batch operations
    batchCreate: jest.fn(),
    batchUpdate: jest.fn(),

    // Field explanation
    explainField: jest.fn(),

    // Configuration validation
    validateCatalogConfiguration: jest.fn(),

    // Update set operations
    inspectUpdateSet: jest.fn(),
    moveRecordsToUpdateSet: jest.fn(),
    cloneUpdateSet: jest.fn(),

    // Workflow operations
    createCompleteWorkflow: jest.fn(),
    createActivity: jest.fn(),
    createTransition: jest.fn(),
    publishWorkflow: jest.fn(),
    createCondition: jest.fn(),

    // Application scope
    setCurrentApplication: jest.fn(),

    // Script synchronization
    syncScript: jest.fn(),
    pullScript: jest.fn(),
    pushScript: jest.fn(),

    // Natural language search
    naturalLanguageSearch: jest.fn(),
  };
}

/**
 * Create mock file system operations
 */
export function createMockFS() {
  return {
    readFile: jest.fn(),
    writeFile: jest.fn(),
    mkdir: jest.fn(),
    access: jest.fn(),
    stat: jest.fn(),
    unlink: jest.fn(),
  };
}

/**
 * Mock incident data
 */
export const mockIncident = {
  sys_id: 'abc123',
  number: 'INC0012345',
  short_description: 'Test incident',
  state: '1',
  priority: '3',
  assigned_to: {
    value: 'user123',
    display_value: 'John Doe'
  },
  assignment_group: {
    value: 'group123',
    display_value: 'IT Support'
  },
  comments: '',
  work_notes: '',
  sys_created_on: '2025-01-01 10:00:00',
  sys_updated_on: '2025-01-01 10:00:00',
};

/**
 * Mock user data
 */
export const mockUser = {
  sys_id: 'user123',
  user_name: 'john.doe',
  name: 'John Doe',
  email: 'john.doe@example.com',
  active: 'true',
};

/**
 * Mock change request data
 */
export const mockChangeRequest = {
  sys_id: 'chg123',
  number: 'CHG0012345',
  short_description: 'Test change',
  state: '1',
  type: 'Normal',
  assigned_to: {
    value: 'user123',
    display_value: 'John Doe'
  },
};

/**
 * Mock problem data
 */
export const mockProblem = {
  sys_id: 'prb123',
  number: 'PRB0012345',
  short_description: 'Test problem',
  state: '1',
  priority: '3',
};

/**
 * Mock table metadata
 */
export const mockTableMetadata = {
  incident: {
    table: 'incident',
    label: 'Incident',
    key_field: 'number',
    display_field: 'short_description',
    required_fields: ['short_description'],
    common_fields: ['number', 'short_description', 'state', 'priority', 'assigned_to'],
    description: 'Incident table for tracking IT issues',
    package: 'Incident Management',
  },
  change_request: {
    table: 'change_request',
    label: 'Change Request',
    key_field: 'number',
    display_field: 'short_description',
    required_fields: ['short_description'],
    common_fields: ['number', 'short_description', 'state', 'type'],
    description: 'Change request table',
    package: 'Change Management',
  },
  problem: {
    table: 'problem',
    label: 'Problem',
    key_field: 'number',
    display_field: 'short_description',
    required_fields: ['short_description'],
    common_fields: ['number', 'short_description', 'state'],
    description: 'Problem table',
    package: 'Problem Management',
  },
};

/**
 * Create mock MCP server
 */
export function createMockMcpServer() {
  return {
    setRequestHandler: jest.fn(),
    connect: jest.fn(),
    close: jest.fn(),
  };
}

/**
 * Helper to create mock axios response
 */
export function createAxiosResponse(data, status = 200) {
  return {
    data,
    status,
    statusText: 'OK',
    headers: {},
    config: {},
  };
}

/**
 * Helper to create mock axios error
 */
export function createAxiosError(message, statusCode = 500) {
  const error = new Error(message);
  error.response = {
    status: statusCode,
    data: { error: { message } },
  };
  return error;
}
