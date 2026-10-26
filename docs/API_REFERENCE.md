# ServiceNow MCP Server - API Reference

**Version:** 2.2
**Last Updated:** 2026-04-19
**Total Tools:** 60

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
11. [Service Catalog AI-Submission Tools](#service-catalog-ai-submission-tools)
12. [ServiceNow Docs Search Tools](#servicenow-docs-search-tools)
13. [ATF Testing Tools (CI/CD REST API)](#atf-testing-tools-cicd-rest-api)
14. [MCP Resources](#mcp-resources)
15. [Multi-Instance Support](#multi-instance-support)

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

### 🛒 **Service Catalog AI-Submission** (4 tools)
Browse, inspect, and submit Service Catalog forms programmatically

### 📚 **ServiceNow Docs Search** (5 tools)
Discover, sync, search, and retrieve official ServiceNowDocs markdown

### 🧪 **ATF Testing (CI/CD REST API)** (5 tools)
Run ATF tests and suites, poll progress, read results and logs

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
  "instance": "prod"               // Optional: Target instance (uses the session's implicit target if omitted)
}
```

**Example:**
```javascript
await client.callTool({
  name: 'SN-Query-Table',
  arguments: {
    table_name: 'incident',
    query: 'active=true^state=1',
    fields: 'number,short_description,priority',
    limit: 10
  }
});
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
await client.callTool({
  name: 'SN-List-Incidents',
  arguments: {
    query: 'state=1^priority=1',
    limit: 10,
    instance: 'prod'
  }
});
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
await client.callTool({
  name: 'SN-Add-Comment',
  arguments: {
    incident_number: 'INC0012345',
    comment: 'User confirmed the issue started after last deployment'
  }
});
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
await client.callTool({
  name: 'SN-Add-Work-Notes',
  arguments: {
    incident_number: 'INC0012345',
    work_notes: 'Applied hotfix to production server'
  }
});
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
await client.callTool({
  name: 'SN-Assign-Incident',
  arguments: {
    incident_number: 'INC0012345',
    assigned_to: 'John Smith',
    assignment_group: 'IT Support'
  }
});
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
await client.callTool({
  name: 'SN-Resolve-Incident',
  arguments: {
    incident_number: 'INC0012345',
    resolution_notes: 'Database connection pool was exhausted. Increased pool size from 10 to 20.',
    resolution_code: 'Solved (Permanently)'
  }
});
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
await client.callTool({
  name: 'SN-Close-Incident',
  arguments: {
    incident_number: 'INC0012345',
    close_notes: 'User confirmed issue is resolved. Monitoring for 24 hours showed no recurrence.',
    close_code: 'Solved (Permanently)'
  }
});
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
await client.callTool({
  name: 'SN-Add-Change-Comment',
  arguments: {
    change_number: 'CHG0012345',
    comment: 'All CAB members have reviewed and approved the change plan'
  }
});
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
await client.callTool({
  name: 'SN-Assign-Change',
  arguments: {
    change_number: 'CHG0012345',
    assigned_to: 'Jane Doe',
    assignment_group: 'Change Management'
  }
});
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
await client.callTool({
  name: 'SN-Approve-Change',
  arguments: {
    change_number: 'CHG0012345',
    approval_comments: 'CAB approved. Proceed with scheduled maintenance window.'
  }
});
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
await client.callTool({
  name: 'SN-Add-Problem-Comment',
  arguments: {
    problem_number: 'PRB0012345',
    comment: 'RCA complete. Issue caused by insufficient connection pool size during peak hours.'
  }
});
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
await client.callTool({
  name: 'SN-Close-Problem',
  arguments: {
    problem_number: 'PRB0012345',
    resolution_notes: 'Permanent fix deployed: increased connection pool from 10 to 50 connections. No incidents reported in 2 weeks.',
    resolution_code: 'Fix Applied'
  }
});
```

---

## Update Set Management

### SN-Get-Current-Update-Set

Get the current user's active update set, read from the ServiceNow update-set picker (`/api/now/ui/concoursepicker/updateset`). The older `/api/now/ui/preferences/sys_update_set` endpoint answers HTTP 400 on current instances and is no longer used.

**Parameters:**
```javascript
{
  "instance": "dev"
}
```

**Returns:**
```json
{
  "result": {
    "name": "My Update Set",
    "value": "0123456789abcdef0123456789abcdef",
    "sys_id": "0123456789abcdef0123456789abcdef"
  }
}
```

---

### SN-Set-Update-Set

Set the current update set through the UI picker, then read the picker back in the same session to verify the change took effect. The response reports the previous update set.

**Parameters:**
```javascript
{
  "update_set_sys_id": "0123456789abcdef0123456789abcdef", // 32 lowercase hex characters
  "instance": "dev"
}
```

**Behavior:**
- An invalid `update_set_sys_id` is rejected with an error before any request.
- If the picker write fails or does not verify, no success is reported: the tool writes a fix script to `./scripts` for manual execution in **Scripts - Background** instead.
- There is no scheduled-job (`sys_trigger`) fallback: a scheduled job runs in the scheduler's own session, so it could not change or verify your current update set.

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
  "update_set_id": "0123456789abcdef0123456789abcdef", // 32 lowercase hex characters
  "source_update_set": "Default",  // Optional: filter by source (≤255 chars, no '^' or control characters)
  "record_sys_ids": ["<sys_id>", "<sys_id>"], // Optional: at most 200 sys_ids
  "time_range": {                   // Optional: time filter
    "start": "2025-09-29 20:00:00",
    "end": "2025-09-29 20:03:31"
  },
  "instance": "dev"
}
```

**Use Case:** Fix records that went to wrong update set

**Limits:** at most 200 `record_sys_ids`, each a 32-character lowercase hex sys_id; `time_range` values must match `YYYY-MM-DD HH:MM:SS`; `table` must be a table name (letters, digits, underscores, ≤80 characters). All inputs are validated before any request. IDs are looked up in `sys_idIN` queries of at most 100 IDs; time-range selections are paginated in `sys_id` order and fail, before any write, if more than 10,000 records match (narrow the range rather than receiving a silently truncated move).

---

### SN-Clone-Update-Set

Clone an entire update set with all records.

**Parameters:**
```javascript
{
  "source_update_set_id": "0123456789abcdef0123456789abcdef",
  "new_name": "Clone of Original",  // 1-255 characters
  "instance": "dev"
}
```

**Limits:** the source sys_id must be 32 lowercase hex characters. Every source `sys_update_xml` record is fetched (paginated) before the clone is created; a source with more than 10,000 records fails without creating an empty clone.

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

**Limits (validated as a whole before the first write):** at most 100 activities and 200 transitions; workflow, activity and condition names ≤255 characters; each script/condition field ≤256 KiB (UTF-8); the whole input ≤4 MiB as JSON; `table` must be a table name; optional `activity_definition_sys_id`/`condition_sys_id` must be 32-character lowercase hex sys_ids. Transition `from`/`to` and `start_activity` must be an activity index, a declared (unique) activity `id`, an activity `name` that is unique among the activities, or an existing 32-character lowercase hex activity sys_id; any other string is rejected.

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

**Limits (validated as a whole before the first write):** at most 100 operations; `table` must be a table name (letters, digits, underscores, ≤80 characters); `data` must be an object of at most 64 KiB as JSON; `save_as` must match `^[A-Za-z_][A-Za-z0-9_]{0,63}$`, be unique within the batch, and not be `__proto__`, `constructor`, `prototype` or a reserved `operation_N` name.

**References:** every `${name}` in an operation's data that names an ID saved by an *earlier* operation (`save_as`, or `operation_N` when `save_as` is omitted) is replaced with that sys_id in a single pass. Unknown, later, malformed (`${name`) or spaced (`${ name }`) references stay literal text, and substituted IDs are never re-scanned.

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
  "transaction": true,  // Stop at the first failed create (earlier records are not rolled back)
  "instance": "dev"
}
```

---

### SN-Batch-Update

Update multiple records efficiently.

**Limits (validated as a whole before the first write):** at most 100 updates; each `sys_id` must be 32 lowercase hex characters; `table` must be a table name; `data` must be an object of at most 64 KiB as JSON.

**Parameters:**
```javascript
{
  "updates": [
    {
      "table": "incident",
      "sys_id": "0123456789abcdef0123456789abcdef",
      "data": { "state": 6 }
    },
    {
      "table": "incident",
      "sys_id": "fedcba9876543210fedcba9876543210",
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

Execute JavaScript server-side the way **System Definition → Scripts - Background** does. Requires the `admin` role. Protocol behavior is ported from `@servicenow/sdk-api` 4.14.0 (`now-sdk exec`, MIT).

**Parameters:**
```javascript
{
  "script": "gs.info('Hello from script');",
  "description": "Backfill missing priority values",  // optional reason
  "scope": "x_acme_app",          // optional: app sys_id, scope name, or "global"
  "record_for_rollback": true,    // optional, default true
  "wait": true,                   // optional, default true
  "instance": "dev"
}
```

With `wait: true` (the default) the script is POSTed **once** to the instance's `/sys.scripts.do` processor as the authenticated user, in a dedicated cookie session that also supplies the `sysparm_ck` CSRF token (the basic-auth user token when the instance exposes one, otherwise that session's `sys_user_session.csrf_token`). The request carries `quota_managed_transaction=on` (the instance cancels a run still going after four hours), `record_for_rollback=on` unless `record_for_rollback: false`, and `sys_scope` when `scope` is given. `scope` is resolved against `sys_scope` first; an unknown scope is rejected before anything runs, because the processor silently runs an unrecognized scope in global. `description` is flattened to one line and appended as a trailing `// happy-platform-mcp: …` comment, so it is recorded with the script on its Script Execution History record.

The processor answers HTTP 200 even for authentication failures, so the HTML response is parsed: the `Script completed in scope …` line proves a result, `*** Script: ` (global) or `<scope>: ` (scoped) lines are output, `Script execution error:` / `Script compilation error:` lines (or a scoped `SyntaxError:`) are errors. The response reports success or failure, output lines, errors, the scope it ran in, and instance-relative links to `/sys_script_execution_history.do?sys_id=…` and, when one was recorded, `/sys_rollback_context.do?sys_id=…` — the run's record changes can be rolled back from that rollback context. Script errors are returned with `isError: true`.

Because the processor runs in the authenticated user's own session (not the scheduler's), that user's current update set is expected to capture tracked configuration writes and `scope` selects the application scope.

The script request is never retried, redirected, or replayed after re-authentication. What happens when it does not return a script result:

| Processor outcome | Script ran? | Action |
|---|---|---|
| HTTP 401 or 403 | No | Fall back to `sys_trigger` |
| HTTP 404 (processor missing) | No | Fall back to `sys_trigger` |
| 3xx redirect to a login/auth page | No | Fall back to `sys_trigger` |
| HTTP 2xx login page, empty body, or `X-Is-Logged-In: false` | No | Fall back to `sys_trigger` |
| Connection refused / DNS / host unreachable before sending | No | Fall back to `sys_trigger` |
| Timeout, HTTP 5xx, other 4xx, non-login redirect, connection dropped after sending, unrecognized 2xx body | Unknown | No fallback; `isError: true` "unknown outcome" — check Script Execution History and do not re-run blindly |

A fallback is refused when a non-global `scope` was requested (the `sys_trigger` path cannot honor it); the script is reported as not run. Every response states which method ran and, for a fallback, why.

**`sys_trigger` path (the fallback, or `wait: false`):** the script runs as a `sys_trigger` scheduled job that fires in ~1 second and auto-deletes after execution. The job runs in the scheduler's own session, not the caller's: tracked configuration writes may be captured into the Default update set (verify `sys_update_xml` and move rows with `SN-Move-Records-To-Update-Set`), `GlideUpdateSet` calls in it cannot change the update set other tools or the UI use, and no rollback context is recorded. `scope` is only accepted with `wait: true`.

As a fallback (`wait: true`), the tool polls `syslog` (bounded by a `sys_created_on` lower bound and a timeout, ~15s by default) for a per-execution correlation marker the script wrapper emits via `gs.info`, then returns one of three outcomes:

- **`completed`** — the script ran without throwing; `output` carries whatever accompanied the success marker, and `logs` carries whatever the script itself logged via `gs.info`/`gs.print` while it ran.
- **`failed`** — the script threw; `error` (and `stack`, when available) carry the failure, and `logs` still carries whatever the script logged before it threw. This is reported as an error, never as success.
- **`timeout`** — the marker never appeared within the budget. `trigger_sys_id` is included so you can inspect the trigger manually; this is never reported as success. There is no end marker to bound a window with, so `logs` is omitted.

`logs` is recovered by bracketing the run with start/end markers and querying `syslog` for everything between the trigger's own `next_action` and the end marker's timestamp, capped at 100 lines (`logsTruncated: true` when more were emitted). A different execution's marker lines are always excluded (matched generically, not by exact text), but two genuinely unrelated scripts logging within the same one-second window on a busy instance are not distinguishable by timestamp alone and could in principle bleed non-marker content in — an inherent platform limitation, not a bug. If this run's own end marker is unexpectedly absent from that window (e.g. clock skew between this host and the instance shifted the bound past the script's real timestamps), the response carries `logsWindowUnverified: true` instead of silently returning an empty `logs` that would look identical to a script that printed nothing.

Pass `wait: false` to skip the processor and polling and get back only trigger scheduling metadata (`trigger_sys_id`, `trigger_name`, `next_action`) immediately, matching the original fire-and-forget behavior (no `logs`, no `output`; see issue #40). If the `sys_trigger` record itself cannot be created, the tool writes a fix script for manual execution instead.

**Returns:** processor path — success/failure, output lines, errors, scope, execution history and rollback context links; `sys_trigger` path — `completed` | `failed` | `timeout` | `scheduled` with trigger details and `output`/`error`/`logs` depending on outcome.

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

The file is written as a new direct child of `./scripts` in the server's working directory, named `<script_name>_<UTC timestamp>.js`, with mode 0600 on POSIX systems (Windows ignores the mode). `script_name` must be 1-100 letters, digits, `.`, `_` or `-`, starting with a letter or digit; path separators, `.`/`..`, drive or absolute paths, control characters and Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9`, `LPT0`-`LPT9`, alone or followed by `.`, case-insensitive) are rejected before anything is written. The write is refused if `./scripts` is a symlink or not a directory, or if the target already exists. `description` and `script_name` are stored in the file as a `fixScriptMetadata` JavaScript data literal, never inside the header comment; `script_content` is written verbatim as the script body.

**Residual limit:** `./scripts` is checked (`lstat`) and then the file is opened in a separate step, with `O_EXCL` and, where available, `O_NOFOLLOW`. `O_NOFOLLOW` only protects the final path component, and Node.js has no `openat`, so someone able to replace `./scripts` with a symlink between those two steps could redirect the new file. That requires write access to the server's working directory, which the operator controls; run the server from a working directory that other users cannot write to.

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

**Via HTTP (when using HTTP transport):** there are no REST resource endpoints. Connect an MCP client to the SSE endpoint with the bearer token, then call `readResource` as above:
```javascript
const transport = new SSEClientTransport(new URL('http://localhost:3000/mcp'), {
  requestInit: { headers: { Authorization: `Bearer ${process.env.HAPPY_MCP_API_TOKEN}` } }
});
await client.connect(transport);
```

---

## Multi-Instance Support

Every live ServiceNow operation accepts an optional `instance` parameter except `SN-Set-Instance`, `SN-Get-Current-Instance`, and `SN-Docs-*`. Omitting it uses the current session client's implicit target. Explicit routing is required when overlapping work may target different instances or race with `SN-Set-Instance`; concurrent calls against one stable target do not require it. Calls with the same explicit instance name share that named cached client.

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

OAuth instances support Client Credentials, Resource Owner Password Credentials, and per-user Authorization Code with PKCE. Tokens are refreshed before expiry and retried once on 401. See `docs/MULTI_INSTANCE_CONFIGURATION.md` for configuration and migration details.

**URL limits:** instance `url`, `authorizeUrl`, `tokenUrl` and `callbackPath` are capped at 4096 characters wherever they enter — the registry file, `SERVICENOW_INSTANCE_URL`/`SERVICENOW_OAUTH_*` environment variables, `SN-Register-Instance`, `SERVICENOW_OAUTH_TRUSTED_ORIGINS` entries, and direct `ServiceNowClient` construction. Longer values are rejected before parsing. Trailing slashes are trimmed with a linear scan (`https://x.service-now.com/tenant///` → `https://x.service-now.com/tenant`); the origin and any path prefix are preserved, and credentials are only attached to requests within that origin and prefix. Instance URLs must be `http(s)` without embedded credentials, query or fragment.

At stdio startup, `SERVICENOW_INSTANCE` selects a named JSON entry when set; otherwise the configured `"default": true` entry is selected, or the first configured entry if none is marked. HTTP sessions use the configured default or first entry. If the JSON file is missing, ServiceNow environment credentials can supply the single fallback instance. `SN-Set-Instance` changes only the current session client's implicit target in memory; it does not modify startup configuration, and a new MCP session or server starts from startup selection again.

### Usage

**Current session implicit target:**
```javascript
await client.callTool({
  name: 'SN-Query-Table',
  arguments: { table_name: 'incident', limit: 10 }
});
```

**Concurrent explicit instances:**
```javascript
await Promise.all([
  client.callTool({
    name: 'SN-Query-Table',
    arguments: { table_name: 'incident', instance: 'dev', limit: 10 }
  }),
  client.callTool({
    name: 'SN-Query-Table',
    arguments: { table_name: 'incident', instance: 'prod', limit: 10 }
  })
]);
```

### List Instances

```bash
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/instances
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

7. **Instance Parameter** to target the correct environment
   - Use explicit per-call `instance` values when overlapping work may target different instances or race with `SN-Set-Instance`
   - Omit `instance` for concurrent calls against one stable session target
   - Use `SN-Set-Instance` only to change the current session's implicit target for later calls

8. **Background Scripts** for complex operations
   - `SN-Execute-Background-Script` runs through `/sys.scripts.do` in your own session, with parsed output and a rollback context
   - Falls back to a `sys_trigger` job only when the processor provably did not run the script
   - Unknown outcome (timeout, 5xx): check Script Execution History, never re-run blindly

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

### ATF Testing (5 tools)
- `SN-Run-ATF-Test` - Run one ATF test and wait for pass/fail (executes on the instance)
- `SN-Run-ATF-Suite` - Run an ATF test suite and wait for counts and per-test status (executes on the instance)
- `SN-Get-CICD-Progress` - Read, or wait on, an sn_cicd progress tracker (read-only)
- `SN-Get-ATF-Results` - Read a test or suite result (read-only)
- `SN-Get-ATF-Test-Logs` - Read a test result's logs, optionally regex-filtered (read-only)

### Application Scope (1 tool)
- `SN-Set-Current-Application` - Set current application scope (sys_app sys_id, or `global`), verified in the same session

### Service Catalog AI-Submission (4 tools)
- `SN-Catalog-Get-Categories` - List catalog categories
- `SN-Catalog-Search-Items` - Search/browse catalog items and record producers
- `SN-Catalog-Get-Item` - Full form context for a catalog item or record producer
- `SN-Catalog-Submit` - Submit a completed catalog form

### ServiceNow Docs Search (5 tools)
- `SN-Docs-Families` - List available ServiceNowDocs families/releases
- `SN-Docs-Status` - Show local docs cache, FTS, and vector status
- `SN-Docs-Sync` - Download and index a docs family locally
- `SN-Docs-Search` - Search locally synced ServiceNowDocs markdown
- `SN-Docs-Get` - Retrieve a markdown document from local cache or GitHub

---

## Service Catalog AI-Submission Tools

These four tools give an AI agent a complete browse → inspect → submit workflow for ServiceNow Service Catalog forms and record producers.

**Recommended workflow:**
```
SN-Catalog-Get-Categories       # discover form families
  → SN-Catalog-Search-Items     # find the right form
    → SN-Catalog-Get-Item       # read field types, choices, mandatory rules
      → SN-Catalog-Submit       # submit with { variable_name: value } map
```

---

### SN-Catalog-Get-Categories

Lists Service Catalog categories (`sc_category`) so the AI can browse available form families before searching for specific items.

**Parameters:**
```javascript
{
  "query":    "active=true",   // Optional: encoded query to filter categories
  "limit":    50,              // Optional: max results (default: 50)
  "instance": "dev"            // Optional: instance name
}
```

**Returns:** Array of `{ sys_id, title, description, parent, active }`.

---

### SN-Catalog-Search-Items

Searches catalog items and record producers by keyword and/or category. Use this to discover available forms before calling `SN-Catalog-Get-Item`.

**Parameters:**
```javascript
{
  "keyword":          "VPN access",  // Optional: matched against name/short_description
  "category":         "<sys_id>",    // Optional: category sys_id
  "active":           true,          // Optional: filter by active status (default: true)
  "limit":            25,            // Optional: max results (default: 25)
  "include_producers": true,         // Optional: also search record producers (default: true)
  "instance":         "dev"          // Optional: instance name
}
```

**Returns:** Array of `{ sys_id, name, short_description, category, active, item_type }` where `item_type` is `"catalog_item"` or `"record_producer"`.

---

### SN-Catalog-Get-Item

Retrieves full form context for a single catalog item or record producer. This is the **"read before you fill"** tool — call this before `SN-Catalog-Submit` to understand which fields exist, what format each expects, and which choices are valid.

**Parameters:**
```javascript
{
  "sys_id":   "<catalog_item_sys_id>",  // Required: catalog item or record producer sys_id
  "instance": "dev"                     // Optional: instance name
}
```

**Returns:**
```javascript
{
  sys_id: "...",
  name: "Request VPN Access",
  short_description: "...",
  description: "...",
  category: "...",
  active: "true",
  item_type: "catalog_item",  // or "record_producer"
  variables: [
    {
      sys_id: "...",
      name: "requested_for",        // Variable name used in SN-Catalog-Submit
      label: "Requested For",       // Display label
      type: "reference",            // Human-readable type (see Variable Types below)
      type_code: "6",
      mandatory: true,
      order: 100,
      default_value: null,
      help_text: "...",
      reference_table: "sys_user",  // Present for reference type variables
      choices: [                    // Present for choice/select/radio/checkbox types
        { text: "Yes", value: "yes" },
        { text: "No",  value: "no"  }
      ]
    }
  ],
  variable_sets: [ { sys_id: "...", display_value: "Common Variables" } ],
  ui_policies: [
    {
      sys_id: "...",
      description: "Make justification mandatory for high-cost items",
      conditions: "...",
      actions: [
        { variable: "justification", mandatory: "true", visible: "true", read_only: "false" }
      ]
    }
  ],
  client_scripts: [
    { name: "Auto-populate department", type: "onChange", applies_to: "..." }
  ]
}
```

**Variable Types** — the `type` field is always a human-readable string:

| `type` | Description | Expected value format |
|---|---|---|
| `single_line_text` | Single-line string | `"string value"` |
| `multi_line_text` | Multi-line string | `"long text..."` |
| `wide_single_line_text` | Wide single-line | `"string value"` |
| `masked` | Password / masked | `"secret"` |
| `email` | Email address | `"user@example.com"` |
| `url` | URL | `"https://..."` |
| `ip_address` | IP address | `"10.0.0.1"` |
| `integer` / `number_scale` | Numeric | `42` |
| `boolean` / `checkbox` | Boolean | `"true"` or `"false"` |
| `date` | Date | `"2026-01-15"` |
| `date_time` | Date + time | `"2026-01-15 09:00:00"` |
| `duration` | Duration | `"1 00:00:00"` (days HH:MM:SS) |
| `reference` | Reference to another table | sys_id string or display value |
| `choice` / `select_box` / `multiple_choice` | Dropdown / radio | one of the `choices[].value` strings |
| `lookup_select_box` / `lookup_multiple_choice` | Table-backed choice | sys_id of the referenced record |
| `multi_select` / `list_collector` | Multi-select | comma-separated values |
| `html` | HTML content | HTML string |

---

### SN-Catalog-Submit

Submits a completed Service Catalog form via the ServiceNow Service Catalog REST API. Works for both catalog items and record producers.

**Parameters:**
```javascript
{
  "sys_id":        "<catalog_item_sys_id>",  // Required
  "variables": {                             // Required: variable_name → value map
    "requested_for":  "jsmith",
    "justification":  "Need VPN for project X",
    "start_date":     "2026-05-01",
    "duration_months": "3"
  },
  "requested_for": "<user_sys_id>",          // Optional: order on behalf of another user
  "quantity":      1,                        // Optional (default: 1)
  "instance":      "dev"                     // Optional: instance name
}
```

**Returns:** The `result` object from ServiceNow's `/api/sn_sc/servicecatalog/items/{sys_id}/order_now` response, which includes the resulting `sc_request` number and related record sys_ids.

---

## ServiceNow Docs Search Tools

These tools use the official `ServiceNow/ServiceNowDocs` markdown repository as the source of truth. Live GitHub discovery/get work without local indexing. Local sync is optional, disabled by default, and builds a SQLite FTS5 index under `~/.happy-platform-mcp/docs/servicenow` when `docs.localIndexEnabled=true` or `HAPPY_DOCS_ENABLE_LOCAL_INDEX=true`. Vector mode is opt-in: set `docs.enableVector=true` or `HAPPY_DOCS_ENABLE_VECTOR=true` with `embeddingProvider=local` to build and query a sqlite-vec index using deterministic local embeddings.

### SN-Docs-Families

Lists available ServiceNow documentation families/releases from GitHub.

**Parameters:**
```javascript
{}
```

**Returns:** Array of `{ name, branch }` family records.

---

### SN-Docs-Status

Shows local docs cache and index status.

**Parameters:**
```javascript
{}
```

**Returns:** Cache path, synced families, FTS availability, vector availability, and vector reason when disabled.

---

### SN-Docs-Sync

Downloads and indexes a ServiceNowDocs family into the local SQLite FTS cache. Defaults to the `australia` family/branch when omitted. Broken markdown links are skipped and reported while the sync continues.

**Parameters:**
```javascript
{
  "family": "australia",   // Optional: family name, defaults to australia
  "branch": "australia"    // Optional: GitHub branch, defaults to family
}
```

**Returns:** Sync summary including family, branch, `documentsSynced`, `documentsSkipped`, and skipped document details.

---

### SN-Docs-Search

Searches locally synced ServiceNowDocs markdown using SQLite FTS5. Defaults to the `australia` family when omitted.

**Parameters:**
```javascript
{
  "query": "create a Flow Designer action", // Required
  "family": "australia",                    // Optional, defaults to australia
  "limit": 10                               // Optional
}
```

**Returns:** Matching chunks with family, path, title, heading, line range, and snippet. If no local results exist, run `SN-Docs-Sync` for the target family.

---

### SN-Docs-Get

Retrieves a ServiceNowDocs markdown document by family and path. Reads from the local cache first, then falls back to GitHub raw markdown. Defaults to the `australia` family when omitted.

**Parameters:**
```javascript
{
  "family": "australia",
  "path": "platform/example.md"
}
```

**Returns:** Document source (`local-cache` or `github`) and markdown content.

---

## ATF Testing Tools (CI/CD REST API)

These tools drive the Automated Test Framework through the ServiceNow CI/CD REST API (`/api/sn_cicd`). The sn_cicd protocol behavior is ported from `@servicenow/sdk-api` 4.14.0 (MIT).

**Prerequisites:** the **CI/CD REST API** plugin (`com.glide.continuousdelivery`) must be active, and the ServiceNow user needs the `sn_cicd.sys_ci_automation` role (or `admin`). ATF execution must be enabled (`sn_atf.runner.enabled` = `true`). UI steps also need a scheduled client test runner or `run_in_cloud: true`. When the plugin is missing (HTTP 400/404 for an unknown URI, or a non-JSON page), tools fail with an error naming the plugin; on HTTP 403 they fail with an error naming the role. When a run tool gets HTTP 400 with a start-failure message (such as "Start ATF test Failed with the given test ID"), it fails with code `ATF_RUNNER_DISABLED`, which says ATF execution may be disabled and to check `sn_atf.runner.enabled`; the same message can also mean an id the instance cannot start.

**Recommended workflow:**
```
SN-Run-ATF-Suite (wait: true)        # starts the run, polls, returns counts + per-test status
  → on status "timeout": SN-Get-CICD-Progress { progress_id, wait: true }
    → SN-Get-ATF-Results { result_id: progress.results.id, result_type: "suite" }
      → SN-Get-ATF-Test-Logs { result_id: <test result sys_id>, pattern: "error" }
```

`SN-Run-ATF-Test` and `SN-Run-ATF-Suite` **execute tests on the instance** (tests may create, change, or roll back data). The other three tools only read.

**Waiting:** with `wait: true` (the default for run tools) the tool polls `GET /api/sn_cicd/progress/{progress_id}` every `poll_interval_seconds` (default 2, range 1-30) for at most `timeout_seconds` (default 45, max 600). The default stays under the 60-second request timeout many MCP clients apply; the tool sends MCP progress notifications on every poll for clients that extend timeouts on progress. When the budget runs out the call returns `status: "timeout"` with the `progress_id` and the last progress state (not an error). The run keeps going on the instance, so continue with `SN-Get-CICD-Progress` and do not start it again. If the run started but its progress cannot be read, the call returns `status: "unknown"` with the `progress_id`.

**Outcome:** the progress tracker decides pass/fail (`successful` for status 2; `failed` and `canceled` are returned with `isError: true`). Result details are fetched afterwards from `links.results.id`; if that fetch fails, `results_error` explains why and the tracker outcome still stands.

---

### SN-Run-ATF-Test

Runs one ATF test: `POST /api/sn_cicd/tests/run_test?test_sys_id=…`.

**Parameters:**
```javascript
{
  "test_sys_id": "0123456789abcdef0123456789abcdef", // or test_name (exactly one)
  "test_name": "Login works",          // resolved via sys_atf_test (name=…, "^" escaped); fails on 0 or 2+ matches
  "run_in_cloud": false,               // Optional: ATF Cloud Runner for UI steps
  "capture_node_logs": false,          // Optional: capture node (browser) logs
  "wait": true,                        // Optional (default true)
  "timeout_seconds": 45,               // Optional (1-600)
  "poll_interval_seconds": 2,          // Optional (1-30)
  "instance": "dev"
}
```

**Returns:** `status` (`successful` | `failed` | `canceled` | `timeout` | `unknown` | `started`), `successful`, `progress_id`, `progress`, and `results` (`test_name`, `test_status`, `output`, `start_time`, `end_time`, `links.api` — the `/api/sn_cicd/tests/test/results/{id}` URL, `links.record` — the `sys_atf_test_result.do` record URL).

---

### SN-Run-ATF-Suite

Runs a test suite: `POST /api/sn_cicd/testsuite/run`. A suite name is passed to the instance as `test_suite_name` and resolved there.

**Parameters:**
```javascript
{
  "test_suite_sys_id": "0123456789abcdef0123456789abcdef", // or test_suite_name (exactly one)
  "test_suite_name": "Smoke Tests",
  "browser_name": "chrome",            // Optional: any | chrome | firefox | edge | ie | safari
  "browser_version": "120",            // Optional (≤100 chars)
  "os_name": "Windows",                // Optional (≤100 chars)
  "os_version": "11",                  // Optional (≤100 chars)
  "run_in_cloud": false,               // Optional
  "is_performance_run": false,         // Optional
  "wait": true, "timeout_seconds": 45, "poll_interval_seconds": 2,
  "instance": "dev"
}
```

**Returns:** the same envelope as `SN-Run-ATF-Test`; `results` holds `test_suite_name`, `test_suite_status`, `counts` (`passed`, `failed`, `errored`, `skipped` from the `rolledup_test_*_count` fields), `child_suite_results`, `links`, and `tests` — per-test rows (`test_name`, `status`, `output`, times, `link`) read from `sys_atf_test_result` where `parent` is the suite result, capped at 200 (`tests_truncated`). If that table read fails, `tests_error` explains why.

---

### SN-Get-CICD-Progress

Reads `GET /api/sn_cicd/progress/{progress_id}`. Read-only.

**Parameters:**
```javascript
{
  "progress_id": "0123456789abcdef0123456789abcdef",
  "wait": false,                       // Optional: true polls until finished or timeout_seconds
  "timeout_seconds": 45, "poll_interval_seconds": 2,
  "instance": "dev"
}
```

**Returns:** `state` (`pending` | `running` | `successful` | `failed` | `canceled`), raw `status`, `status_label`, `status_message`, `status_detail`, `error`, `percent_complete`, `finished`, and `results` (`id`, `url`) once available. With `wait: true`, `timed_out: true` marks a run that is still going.

---

### SN-Get-ATF-Results

Reads a result. Read-only.

**Parameters:**
```javascript
{
  "result_id": "0123456789abcdef0123456789abcdef",
  "result_type": "suite",              // "test" → GET /api/sn_cicd/tests/test/results/{id}
                                       // "suite" → GET /api/sn_cicd/testsuite/results/{id} + sys_atf_test_result rows
  "instance": "dev"
}
```

**Returns:** the `results` object described under the run tools.

---

### SN-Get-ATF-Test-Logs

Reads `GET /api/sn_cicd/tests/test/results/{result_id}/logs?sysparm_regex=…&sysparm_limit=…`. Read-only.

**Parameters:**
```javascript
{
  "result_id": "0123456789abcdef0123456789abcdef",
  "pattern": "error|fail",             // Optional regex; the instance matches it case-insensitively
  "limit": 100,                        // Optional (1-1000, default 100)
  "instance": "dev"
}
```

`pattern` is validated before any request: at most 256 characters, no control characters, must compile as a JavaScript regular expression, and a quantified group that is itself quantified (such as `(a+)+` or `(.*)*`) is rejected to avoid catastrophic backtracking. The instance's regex dialect may differ slightly from JavaScript.

**Returns:** `result_id`, `pattern`, `limit`, and `logs` (the instance's response as-is). A response larger than 256 KiB is returned as a truncated `logs_text` string with `logs_truncated: true`. When the instance answers HTTP 404 "No logs found for test result …", the call succeeds with `logs: []` and a `note`: logs are captured only when the run used `capture_node_logs: true`. Other 404s stay errors.

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

**Total Tools:** 60
**Total Resources:** 2
**Supported Tables:** 160+
**Instance Support:** Unlimited (via config)

**Key Features:**
- Generic tools work on any ServiceNow table
- Convenience tools for better UX (accept numbers/names instead of sys_ids)
- Automated background script execution via the `/sys.scripts.do` processor (with `sys_trigger` fallback)
- Multi-instance support with easy switching
- Batch operations with progress notifications
- MCP resources for metadata discovery
- Comprehensive schema introspection
- Full Service Catalog AI-submission workflow (browse → inspect → submit)
