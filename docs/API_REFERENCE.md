# ServiceNow MCP Server - API Reference

**Version:** 2.1
**Last Updated:** 2025-10-06
**Total Tools:** 44

Complete reference for all MCP tools and resources available in the ServiceNow server.

---

## Table of Contents

1. [Tool Categories](#tool-categories)
2. [Generic CRUD Operations](#generic-crud-operations)
3. [Specialized Tools](#specialized-tools)
4. [Incident Management Convenience Tools](#incident-management-convenience-tools)
5. [Change Request Convenience Tools](#change-request-convenience-tools)
6. [Problem Management Convenience Tools](#problem-management-convenience-tools)
7. [Update Set Management](#update-set-management)
8. [Workflow Operations](#workflow-operations)
9. [Schema & Discovery](#schema--discovery)
10. [Batch Operations](#batch-operations)
11. [MCP Resources](#mcp-resources)
12. [Multi-Instance Support](#multi-instance-support)

---

## Tool Categories

### 📊 **Generic CRUD Operations** (6 tools)
Work on **any** ServiceNow table (160+ supported)

### 🎯 **Specialized Tools** (6 tools)
Table-specific list operations for core ITSM tables

### 🎫 **Incident Management Convenience Tools** (5 tools)
User-friendly incident operations accepting incident numbers

### 🔄 **Change Request Convenience Tools** (3 tools)
User-friendly change request operations accepting change numbers

### ⚠️ **Problem Management Convenience Tools** (2 tools)
User-friendly problem operations accepting problem numbers

### 📦 **Update Set Management** (6 tools)
Advanced update set operations

### 🌊 **Workflow Operations** (4 tools)
Create and manage workflows programmatically

### 🔍 **Schema & Discovery** (4 tools)
Table introspection and metadata

### ⚡ **Batch Operations** (2 tools)
Efficient multi-record operations

### 🔌 **Instance Management** (2 tools)
Multi-instance configuration and switching

### 🛠️ **Script Execution** (2 tools)
Automated and manual background script execution

### 📊 **Advanced Validation** (2 tools)
Configuration validation and field explanation

---

## Generic CRUD Operations

These tools work on **any** ServiceNow table.

### SN-Query-Table

Query records from any table with advanced filtering.

**Parameters:**
```javascript
{
  "table_name": "incident",        // Required: ServiceNow table name
  "query": "state=1^priority=1",   // Optional: Encoded query string
  "fields": "number,short_description", // Optional: Comma-separated fields
  "limit": 25,                     // Optional: Max records (default: 25)
  "offset": 0,                     // Optional: Skip records for pagination
  "order_by": "sys_created_on",    // Optional: Sort field (prefix with - for desc)
  "instance": "prod"               // Optional: Target instance (uses default if omitted)
}
```

**Example:**
```javascript
SN-Query-Table({
  "table_name": "incident",
  "query": "active=true^state=1",
  "fields": "number,short_description,priority",
  "limit": 10
})
```

---

### SN-Create-Record

Create a record in any table.

**Parameters:**
```javascript
{
  "table_name": "incident",
  "data": {
    "short_description": "Email server down",
    "urgency": 1,
    "impact": 2
  },
  "instance": "dev"
}
```

**Returns:** Created record with sys_id

---

### SN-Get-Record

Get a single record by sys_id.

**Parameters:**
```javascript
{
  "table_name": "incident",
  "sys_id": "abc123...",
  "fields": "number,state,priority",  // Optional
  "instance": "prod"
}
```

---

### SN-Update-Record

Update an existing record.

**Parameters:**
```javascript
{
  "table_name": "incident",
  "sys_id": "abc123...",
  "data": {
    "state": 6,
    "resolution_notes": "Issue resolved"
  },
  "instance": "prod"
}
```

---

## Specialized Tools

### Core ITSM Tables

Each major table has specialized tools:

- **Incidents:** `SN-List-Incidents`, `SN-Create-Incident`, `SN-Get-Incident`
- **Changes:** `SN-List-ChangeRequests`
- **Problems:** `SN-List-Problems`
- **Users:** `SN-List-SysUsers`
- **Groups:** `SN-List-SysUserGroups`
- **CMDB:** `SN-List-CmdbCis`

**Example:**
```javascript
SN-List-Incidents({
  "query": "state=1^priority=1",
  "limit": 10,
  "instance": "prod"
})
```

---

## Incident Management Convenience Tools

User-friendly tools that accept incident numbers instead of sys_ids.

### SN-Add-Comment

Add a comment to an incident using the incident number.

**Parameters:**
```javascript
{
  "incident_number": "INC0012345",  // Required
  "comment": "Investigating the issue with the user",
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Add-Comment({
  "incident_number": "INC0012345",
  "comment": "User confirmed the issue started after last deployment"
})
```

---

### SN-Add-Work-Notes

Add work notes to an incident (internal notes not visible to users).

**Parameters:**
```javascript
{
  "incident_number": "INC0012345",  // Required
  "work_notes": "Checked logs, found database connection timeout",
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Add-Work-Notes({
  "incident_number": "INC0012345",
  "work_notes": "Applied hotfix to production server"
})
```

---

### SN-Assign-Incident

Assign an incident to a user and/or group. Automatically resolves user names to sys_ids.

**Parameters:**
```javascript
{
  "incident_number": "INC0012345",  // Required
  "assigned_to": "John Smith",  // User name or sys_id
  "assignment_group": "Network Team",  // Optional: Group name or sys_id
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Assign-Incident({
  "incident_number": "INC0012345",
  "assigned_to": "John Smith",
  "assignment_group": "IT Support"
})
```

**Features:**
- Accepts user name or sys_id for `assigned_to`
- Accepts group name or sys_id for `assignment_group`
- Automatically looks up sys_ids from names

---

### SN-Resolve-Incident

Resolve an incident with resolution notes.

**Parameters:**
```javascript
{
  "incident_number": "INC0012345",  // Required
  "resolution_notes": "Restarted service, issue resolved",
  "resolution_code": "Solved (Permanently)",  // Optional
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Resolve-Incident({
  "incident_number": "INC0012345",
  "resolution_notes": "Database connection pool was exhausted. Increased pool size from 10 to 20.",
  "resolution_code": "Solved (Permanently)"
})
```

**Resolution Codes:**
- "Solved (Permanently)"
- "Solved (Work Around)"
- "Not Solved (Not Reproducible)"
- "Not Solved (Too Costly)"

---

### SN-Close-Incident

Close an incident with close notes.

**Parameters:**
```javascript
{
  "incident_number": "INC0012345",  // Required
  "close_notes": "Confirmed fix is working in production",
  "close_code": "Solved (Permanently)",  // Optional
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Close-Incident({
  "incident_number": "INC0012345",
  "close_notes": "User confirmed issue is resolved. Monitoring for 24 hours showed no recurrence.",
  "close_code": "Solved (Permanently)"
})
```

---

## Change Request Convenience Tools

User-friendly tools for managing change requests.

### SN-Add-Change-Comment

Add a comment to a change request.

**Parameters:**
```javascript
{
  "change_number": "CHG0012345",  // Required
  "comment": "Risk assessment completed",
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Add-Change-Comment({
  "change_number": "CHG0012345",
  "comment": "All CAB members have reviewed and approved the change plan"
})
```

---

### SN-Assign-Change

Assign a change request to a user and/or group.

**Parameters:**
```javascript
{
  "change_number": "CHG0012345",  // Required
  "assigned_to": "Jane Doe",  // User name or sys_id
  "assignment_group": "Change Management",  // Optional
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Assign-Change({
  "change_number": "CHG0012345",
  "assigned_to": "Jane Doe",
  "assignment_group": "Change Management"
})
```

---

### SN-Approve-Change

Approve a change request.

**Parameters:**
```javascript
{
  "change_number": "CHG0012345",  // Required
  "approval_comments": "Risk is acceptable, approved for production",  // Optional
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Approve-Change({
  "change_number": "CHG0012345",
  "approval_comments": "CAB approved. Proceed with scheduled maintenance window."
})
```

---

## Problem Management Convenience Tools

User-friendly tools for managing problems.

### SN-Add-Problem-Comment

Add a comment to a problem record.

**Parameters:**
```javascript
{
  "problem_number": "PRB0012345",  // Required
  "comment": "Root cause identified in database configuration",
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Add-Problem-Comment({
  "problem_number": "PRB0012345",
  "comment": "RCA complete. Issue caused by insufficient connection pool size during peak hours."
})
```

---

### SN-Close-Problem

Close a problem with resolution information.

**Parameters:**
```javascript
{
  "problem_number": "PRB0012345",  // Required
  "resolution_notes": "Database pool size increased, monitoring confirms fix",
  "resolution_code": "Fix Applied",  // Optional
  "instance": "prod"  // Optional
}
```

**Example:**
```javascript
SN-Close-Problem({
  "problem_number": "PRB0012345",
  "resolution_notes": "Permanent fix deployed: increased connection pool from 10 to 50 connections. No incidents reported in 2 weeks.",
  "resolution_code": "Fix Applied"
})
```

---

## Update Set Management

### SN-Get-Current-Update-Set

Get the currently active update set.

**Parameters:**
```javascript
{
  "instance": "dev"
}
```

**Returns:**
```json
{
  "sys_id": "abc123...",
  "name": "My Update Set",
  "state": "in progress"
}
```

---

### SN-Set-Update-Set

Set the current update set programmatically.

**Parameters:**
```javascript
{
  "update_set_sys_id": "abc123...",
  "instance": "dev"
}
```

**Implementation:** Uses automated background script execution via `sys_trigger`

---

### SN-List-Update-Sets

List available update sets with filtering.

**Parameters:**
```javascript
{
  "query": "state=in progress",
  "limit": 25,
  "order_by": "-sys_created_on",
  "instance": "dev"
}
```

---

### SN-Move-Records-To-Update-Set

Move sys_update_xml records between update sets.

**Parameters:**
```javascript
{
  "update_set_id": "target_sys_id",
  "source_update_set": "Default",  // Optional: filter by source
  "record_sys_ids": ["id1", "id2"], // Optional: specific records
  "time_range": {                   // Optional: time filter
    "start": "2025-09-29 20:00:00",
    "end": "2025-09-29 20:03:31"
  },
  "instance": "dev"
}
```

**Use Case:** Fix records that went to wrong update set

---

### SN-Clone-Update-Set

Clone an entire update set with all records.

**Parameters:**
```javascript
{
  "source_update_set_id": "abc123...",
  "new_name": "Clone of Original",
  "instance": "dev"
}
```

---

### SN-Inspect-Update-Set

Inspect update set contents and dependencies.

**Parameters:**
```javascript
{
  "update_set": "abc123...",
  "show_components": true,
  "show_dependencies": false,
  "instance": "dev"
}
```

---

## Workflow Operations

### SN-Create-Workflow

Create a complete workflow with activities and transitions.

**Parameters:**
```javascript
{
  "name": "Auto-Approve Change",
  "table": "change_request",
  "description": "Automatically approve low-risk changes",
  "activities": [
    {
      "name": "Check Risk",
      "script": "if (current.risk == '4') { answer = 'yes'; }"
    },
    {
      "name": "Auto Approve",
      "script": "current.approval = 'approved'; current.update();"
    }
  ],
  "transitions": [
    {
      "from": "Check Risk",
      "to": "Auto Approve",
      "condition_script": "answer == 'yes'"
    }
  ],
  "publish": false,
  "instance": "dev"
}
```

---

### SN-Create-Activity

Create a single workflow activity.

**Parameters:**
```javascript
{
  "workflow_version_sys_id": "abc123...",
  "name": "Send Notification",
  "script": "gs.eventQueue('incident.resolved', current);",
  "x": 100,  // Canvas position
  "y": 100,
  "instance": "dev"
}
```

---

### SN-Publish-Workflow

Publish a workflow version.

**Parameters:**
```javascript
{
  "version_sys_id": "abc123...",
  "start_activity_sys_id": "def456...",
  "instance": "dev"
}
```

---

## Schema & Discovery

### SN-Get-Table-Schema

Get basic table schema information.

**Parameters:**
```javascript
{
  "table_name": "incident",
  "instance": "prod"
}
```

**Returns:** Field names, types, labels

---

### SN-Discover-Table-Schema

Get comprehensive table metadata with relationships.

**Parameters:**
```javascript
{
  "table_name": "sys_hub_flow",
  "include_relationships": true,
  "include_field_constraints": true,
  "include_type_codes": true,
  "include_choice_tables": true,
  "include_ui_policies": false,
  "include_business_rules": false,
  "instance": "dev"
}
```

**Returns:** Complete schema with:
- Field definitions
- Reference relationships
- Choice tables
- Field constraints
- Type codes

---

### SN-List-Available-Tables

List all available ServiceNow tables.

**Parameters:**
```javascript
{
  "category": "core_itsm",  // Optional: filter by category
  "instance": "prod"
}
```

**Categories:** `core_itsm`, `platform`, `service_catalog`, `cmdb`, `all`

---

### SN-Explain-Field

Get detailed explanation of a specific field.

**Parameters:**
```javascript
{
  "table": "catalog_ui_policy_action",
  "field": "catalog_variable",
  "include_examples": true,
  "instance": "dev"
}
```

---

## Batch Operations

### SN-Batch-Create

Create multiple records in a single operation.

**Parameters:**
```javascript
{
  "operations": [
    {
      "table": "incident",
      "data": { "short_description": "Issue 1" },
      "save_as": "incident1"
    },
    {
      "table": "incident_task",
      "data": {
        "parent": "${incident1}",  // Reference previous record
        "short_description": "Task 1"
      }
    }
  ],
  "transaction": true,  // All-or-nothing
  "instance": "dev"
}
```

---

### SN-Batch-Update

Update multiple records efficiently.

**Parameters:**
```javascript
{
  "updates": [
    {
      "table": "incident",
      "sys_id": "abc123...",
      "data": { "state": 6 }
    },
    {
      "table": "incident",
      "sys_id": "def456...",
      "data": { "state": 6 }
    }
  ],
  "stop_on_error": false,
  "instance": "dev"
}
```

---

## Background Script Execution

### SN-Execute-Background-Script

Execute JavaScript server-side with automated sys_trigger execution.

**Parameters:**
```javascript
{
  "script": "gs.info('Hello from script');",
  "description": "Test script execution",
  "execution_method": "trigger",  // trigger (default), ui, or auto
  "instance": "dev"
}
```

**Execution Methods:**
- `trigger` (recommended): Uses sys_trigger, runs in ~1 second, auto-deletes
- `ui`: Attempts direct UI endpoint execution
- `auto`: Tries trigger, then ui, then creates fix script

**Returns:** Success status and trigger details

---

### SN-Create-Fix-Script

Generate a script file for manual execution (fallback).

**Parameters:**
```javascript
{
  "script_name": "link_ui_policy_actions",
  "script_content": "var gr = new GlideRecord('...'); gr.update();",
  "description": "Link UI policy actions to policies",
  "auto_delete": true,  // Delete script file after execution
  "instance": "dev"
}
```

**Use Case:** When automated execution is not available

---

## MCP Resources

MCP Resources provide read-only access to ServiceNow metadata and configuration information. Unlike tools (which perform actions), resources return static or semi-static data that can be cached and referenced.

### Understanding Resources vs Tools

**Tools:**
- Perform actions (create, update, delete)
- Accept parameters and return results
- Execute operations that modify state
- Example: `SN-Create-Incident`, `SN-Update-Record`

**Resources:**
- Provide read-only data access
- Can be cached by the MCP client
- Return metadata and configuration
- Example: Instance info, table schemas

### Available Resources

#### servicenow://instance

Returns information about the currently connected ServiceNow instance.

**URI:** `servicenow://instance`

**Returns:**
```json
{
  "server_info": {
    "name": "ServiceNow MCP Server (Consolidated)",
    "version": "2.0.0",
    "description": "Consolidated ServiceNow integration with metadata-driven schema lookups"
  },
  "instance_info": {
    "url": "https://dev123.service-now.com",
    "username": "admin"
  },
  "capabilities": {
    "total_tables": 160,
    "operations": ["create", "read", "update", "query", "schema_lookup"],
    "tools": 44
  }
}
```

**Usage:**
- Check current connection status
- Verify instance configuration
- View available capabilities

---

#### servicenow://tables/all

Returns complete metadata for all available ServiceNow tables.

**URI:** `servicenow://tables/all`

**Returns:**
```json
{
  "incident": {
    "name": "incident",
    "label": "Incident",
    "key_field": "number",
    "required_fields": ["short_description"],
    "common_fields": ["number", "short_description", "description", "priority", "state"],
    "description": "ITSM incident management table"
  },
  "change_request": {
    "name": "change_request",
    "label": "Change Request",
    "key_field": "number",
    "required_fields": ["short_description"],
    "common_fields": ["number", "short_description", "risk", "impact"],
    "description": "Change management requests"
  }
  // ... 160+ more tables
}
```

**Usage:**
- Discover available tables
- View required and common fields
- Find key fields for each table
- Build dynamic queries and forms

---

### Reading Resources

**Via MCP SDK:**
```javascript
// Read instance information
const instance = await client.readResource('servicenow://instance');

// Read all table metadata
const tables = await client.readResource('servicenow://tables/all');
```

**Via HTTP (when using HTTP transport):**
```bash
# List available resources
curl http://localhost:3000/mcp/resources

# Read specific resource
curl http://localhost:3000/mcp/resources/servicenow://instance
```

---

## Multi-Instance Support

All tools support the `instance` parameter to route requests to specific ServiceNow instances.

### Configuration

Set up multiple instances in `config/servicenow-instances.json`. Each instance can use Basic Auth (default) or OAuth 2.0:

```json
{
  "instances": [
    {
      "name": "dev",
      "url": "https://dev123.service-now.com",
      "username": "admin",
      "password": "password",
      "default": true
    },
    {
      "name": "prod",
      "url": "https://prod456.service-now.com",
      "authType": "oauth",
      "grantType": "client_credentials",
      "clientId": "your-client-id",
      "clientSecret": "your-client-secret"
    }
  ]
}
```

OAuth instances use the Resource Owner Password Credentials grant against `/oauth_token.do`. Tokens are cached, auto-refreshed, and retried on 401. See `docs/MULTI_INSTANCE_CONFIGURATION.md` for full details.

### Usage

**Default Instance:**
```javascript
SN-List-Incidents({ "limit": 10 })
```

**Specific Instance:**
```javascript
SN-List-Incidents({ "limit": 10, "instance": "prod" })
```

### List Instances

```bash
curl http://localhost:3000/instances
```

---

## Error Handling

All tools return errors in this format:

```json
{
  "error": "Error message",
  "details": "Detailed explanation",
  "instance": "dev",
  "table": "incident"
}
```

**Common Errors:**
- `400` - Invalid parameters
- `401` - Authentication failed
- `403` - Permission denied
- `404` - Record/table not found
- `500` - ServiceNow API error

---

## Rate Limiting

ServiceNow enforces rate limits on API calls:
- **Standard:** 1000 requests per hour per user
- **Batch operations** count as multiple requests
- Use pagination with `limit` and `offset` for large datasets

---

## Best Practices

1. **Use Convenience Tools** for common ITSM operations (SN-Add-Comment, SN-Assign-Incident, etc.)
   - Accept human-readable identifiers (incident numbers, user names)
   - Automatically resolve sys_ids
   - Provide better error messages

2. **Use Generic Tools** (`SN-Query-Table`) for flexibility and custom tables
   - Works on any ServiceNow table
   - Required for tables without specialized tools

3. **Batch Operations** for multiple record operations
   - Up to 50+ operations in a single call
   - Transactional support for data integrity
   - Progress notifications for long-running operations

4. **Field Selection** to reduce payload size
   - Use `fields` parameter to specify only needed fields
   - Reduces network transfer and improves performance
   - Example: `fields: "number,short_description,state"`

5. **Pagination** for large result sets
   - Use `limit` and `offset` for controlled data retrieval
   - Default limit is 25, maximum is typically 1000
   - Implement pagination for tables with many records

6. **Update Sets** to track configuration changes
   - Always set update set BEFORE making config changes
   - Use `SN-Set-Update-Set` for automated setup
   - Verify with `SN-Get-Current-Update-Set`

7. **Instance Parameter** to target correct environment
   - Specify `instance` parameter for multi-instance setups
   - Default instance used if not specified
   - Use `SN-Set-Instance` or `SN-Get-Current-Instance` for management

8. **Background Scripts** for complex operations
   - Use `SN-Execute-Background-Script` with trigger method
   - Executes in ~1 second, fully automated
   - Fallback to fix script if needed

9. **MCP Resources** for metadata discovery
   - Cache resource data to avoid repeated queries
   - Use `servicenow://tables/all` for table discovery
   - Check `servicenow://instance` for connection verification

---

## Quick Reference - All Tools

### Generic CRUD (6 tools)
- `SN-Query-Table` - Query any table with filters
- `SN-Create-Record` - Create record in any table
- `SN-Get-Record` - Get single record by sys_id
- `SN-Update-Record` - Update existing record
- `SN-Get-Table-Schema` - Get basic table schema
- `SN-List-Available-Tables` - List all available tables

### Specialized List Tools (6 tools)
- `SN-List-Incidents` - List incidents with filters
- `SN-Create-Incident` - Create new incident
- `SN-Get-Incident` - Get incident by sys_id
- `SN-List-ChangeRequests` - List change requests
- `SN-List-Problems` - List problems
- `SN-List-SysUsers` - List users
- `SN-List-SysUserGroups` - List user groups
- `SN-List-CmdbCis` - List configuration items

### Incident Convenience (5 tools)
- `SN-Add-Comment` - Add comment by incident number
- `SN-Add-Work-Notes` - Add work notes by incident number
- `SN-Assign-Incident` - Assign incident (resolves user names)
- `SN-Resolve-Incident` - Resolve incident with notes
- `SN-Close-Incident` - Close incident with notes

### Change Request Convenience (3 tools)
- `SN-Add-Change-Comment` - Add comment by change number
- `SN-Assign-Change` - Assign change request
- `SN-Approve-Change` - Approve change request

### Problem Convenience (2 tools)
- `SN-Add-Problem-Comment` - Add comment by problem number
- `SN-Close-Problem` - Close problem with resolution

### Update Set Management (6 tools)
- `SN-Get-Current-Update-Set` - Get active update set
- `SN-Set-Update-Set` - Set current update set (automated)
- `SN-List-Update-Sets` - List all update sets
- `SN-Move-Records-To-Update-Set` - Move records between sets
- `SN-Clone-Update-Set` - Clone entire update set
- `SN-Inspect-Update-Set` - Inspect update set contents

### Workflow Operations (4 tools)
- `SN-Create-Workflow` - Create complete workflow
- `SN-Create-Activity` - Add workflow activity
- `SN-Create-Transition` - Link workflow activities
- `SN-Publish-Workflow` - Publish workflow version

### Schema & Discovery (4 tools)
- `SN-Discover-Table-Schema` - Deep schema with relationships
- `SN-Explain-Field` - Detailed field documentation
- `SN-Validate-Configuration` - Validate catalog config
- `SN-Inspect-Update-Set` - Inspect update set (also in Update Set)

### Batch Operations (2 tools)
- `SN-Batch-Create` - Create multiple records with references
- `SN-Batch-Update` - Update multiple records efficiently

### Instance Management (2 tools)
- `SN-Set-Instance` - Switch to different instance
- `SN-Get-Current-Instance` - Get current instance info

### Script Execution (2 tools)
- `SN-Execute-Background-Script` - Automated script execution
- `SN-Create-Fix-Script` - Generate script for manual execution

### Application Scope (1 tool)
- `SN-Set-Current-Application` - Set current application scope

---

## MCP Resources (2 resources)

- `servicenow://instance` - Instance info and capabilities
- `servicenow://tables/all` - Complete table metadata

---

## Additional Documentation

- **Setup Guide:** `docs/SETUP_GUIDE.md`
- **Multi-Instance Config:** `docs/MULTI_INSTANCE_CONFIGURATION.md`
- **Instance Switching:** `docs/INSTANCE_SWITCHING_GUIDE.md`
- **Troubleshooting:** `docs/403_TROUBLESHOOTING.md`
- **Research & Breakthroughs:** `docs/research/`

---

## Summary

**Total Tools:** 44
**Total Resources:** 2
**Supported Tables:** 160+
**Instance Support:** Unlimited (via config)

**Key Features:**
- Generic tools work on any ServiceNow table
- Convenience tools for better UX (accept numbers/names instead of sys_ids)
- Automated background script execution via sys_trigger
- Multi-instance support with easy switching
- Batch operations with progress notifications
- MCP resources for metadata discovery
- Comprehensive schema introspection