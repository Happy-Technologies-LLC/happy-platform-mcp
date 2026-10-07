# ServiceNow MCP Server v2.0 - Development Guide

**For:** Claude Code (claude.ai/code)
**Version:** 2.0
**Instance:** Multi-instance support enabled

---

## 🚨 CRITICAL: MCP-FIRST POLICY

**ALWAYS use MCP tools as the PRIMARY method for ServiceNow operations.**

### Priority Order (MANDATORY):
1. **FIRST**: Use MCP tools (SN-Create-Record, SN-Update-Record, SN-Query-Table, etc.)
2. **SECOND**: If MCP fails, use SN-Execute-Background-Script
3. **LAST RESORT**: Document as manual step only if both fail

### When to Write Background Scripts:
- **NEVER** as first choice
- **ONLY** when MCP tools have documented limitations
- **ALWAYS** try MCP batch operations first (5-10+ parallel calls)

---

## 🎯 MCP Tools (40+ Total)

### Generic Tools (Work on ANY table)
**Core CRUD:**
- `SN-Query-Table` - Query records with filtering
- `SN-Create-Record` - Create records in any table
- `SN-Get-Record` - Get single record by sys_id
- `SN-Update-Record` - Update existing records
- `SN-Get-Table-Schema` - Get table structure
- `SN-Discover-Table-Schema` - Deep schema with relationships
- `SN-List-Available-Tables` - List all available tables

### Specialized Tables (Convenience)
**ITSM:**
- `SN-List-Incidents`, `SN-Create-Incident`, `SN-Get-Incident`
- `SN-List-ChangeRequests`
- `SN-List-Problems`

**Administration:**
- `SN-List-SysUsers`
- `SN-List-SysUserGroups`
- `SN-List-CmdbCis`

### Convenience Tools (Incident Operations)
- `SN-Update-Incident` - Update incident by sys_id
- `SN-Close-Incident` - Close incident with resolution
- `SN-Assign-Incident` - Assign incident to user/group
- `SN-Add-Work-Notes` - Add work notes to incident
- `SN-Search-Incidents` - Natural language incident search

### Update Set Management
- `SN-Set-Update-Set` - Set current update set (AUTOMATED!)
- `SN-Get-Current-Update-Set` - Get active update set
- `SN-List-Update-Sets` - List available update sets
- `SN-Move-Records-To-Update-Set` - Fix records in wrong set
- `SN-Clone-Update-Set` - Clone entire update set
- `SN-Inspect-Update-Set` - Inspect update set contents

### Application Scope
- `SN-Set-Current-Application` - Set current application scope (AUTOMATED!)

### Script Execution
- `SN-Execute-Background-Script` - Execute server-side JavaScript (via sys_trigger)
- `SN-Create-Fix-Script` - Generate script for manual execution

### Script Synchronization
- `SN-Sync-Script-To-Local` - Download script to local file
- `SN-Sync-Local-To-Script` - Upload local file to ServiceNow
- `SN-Watch-Script` - Watch local file and auto-sync changes

### Natural Language Interface
- `SN-NL-Search` - Search records using natural language
- `SN-NL-Query-Builder` - Convert NL to encoded query

### Batch Operations
- `SN-Batch-Create` - Create multiple records with relationships
- `SN-Batch-Update` - Update multiple records efficiently

### Workflow Operations
- `SN-Create-Workflow` - Create complete workflow with activities
- `SN-Create-Activity` - Add activity to workflow
- `SN-Create-Transition` - Link workflow activities
- `SN-Publish-Workflow` - Publish workflow version

### Advanced Tools
- `SN-Validate-Configuration` - Validate catalog item config
- `SN-Explain-Field` - Get field documentation

### MCP Resources (Read-Only Access)
- `servicenow://instances` - List available instances
- `servicenow://schema/{table}` - Table schema as resource
- `servicenow://scripts/{sys_id}` - Script include contents

**Complete API Reference:** `docs/API_REFERENCE.md`

---

## 🌐 Multi-Instance Support

All tools support the `instance` parameter for routing to specific ServiceNow instances.

**Configuration:** `config/servicenow-instances.json`

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
      "username": "integration",
      "password": "password"
    }
  ]
}
```

**Usage:**
```javascript
// Default instance (marked with "default": true)
SN-Query-Table({ table_name: "incident", limit: 10 })

// Specific instance
SN-Query-Table({ table_name: "incident", limit: 10, instance: "prod" })
```

---

## 🎉 BREAKTHROUGH: Automated Background Script Execution

**As of 2025-09-29**, discovered automated background script execution via `sys_trigger` table:

### SN-Execute-Background-Script (FULLY AUTOMATED!)
```javascript
SN-Execute-Background-Script({
  script: "gs.info('Hello from automated script');",
  description: "Test automated execution",
  execution_method: "trigger"  // Default, most reliable
});
```

**How it works:**
1. Creates scheduled job in `sys_trigger` table
2. Executes in ~1 second
3. Auto-deletes trigger after execution
4. No manual copy-paste required!

**Fallback methods:**
- `ui`: Direct UI endpoint execution
- `auto`: Try trigger → ui → create fix script

### SN-Set-Update-Set (FULLY AUTOMATED!)
```javascript
// Set current update set programmatically
SN-Set-Update-Set({ update_set_sys_id: "abc123..." });
```

Uses automated background script execution - takes ~2 seconds, fully scriptable!

### SN-Set-Current-Application (FULLY AUTOMATED!)
```javascript
// Set application scope for scoped app development
SN-Set-Current-Application({ app_sys_id: "def456..." });
```

Enables **fully automated scoped application development** with zero manual steps!

---

## 🎨 Local Development Workflow

### Script Synchronization (NEW!)
Develop ServiceNow scripts locally with version control and modern IDE features:

```javascript
// 1. Download script to local file
SN-Sync-Script-To-Local({
  script_sys_id: "abc123...",
  local_path: "/scripts/business_rules/validate_incident.js",
  instance: "dev"
});

// 2. Edit locally with your IDE (syntax highlighting, linting, etc.)
// File: /scripts/business_rules/validate_incident.js

// 3. Upload changes back to ServiceNow
SN-Sync-Local-To-Script({
  local_path: "/scripts/business_rules/validate_incident.js",
  script_sys_id: "abc123...",
  instance: "dev"
});

// 4. OR: Watch for changes and auto-sync
SN-Watch-Script({
  local_path: "/scripts/business_rules/validate_incident.js",
  script_sys_id: "abc123...",
  instance: "dev"
});
// Now edit the file - changes sync automatically every 2 seconds!
```

### Git Integration Example
```bash
# Directory structure for version-controlled ServiceNow scripts
scripts/
├── business_rules/
│   ├── validate_incident.js
│   └── auto_assign.js
├── script_includes/
│   ├── IncidentUtils.js
│   └── NotificationHelper.js
└── ui_scripts/
    └── form_validation.js

# Track changes with git
git add scripts/
git commit -m "Update incident validation logic"
git push origin feature/incident-validation

# Deploy to ServiceNow
# Use SN-Sync-Local-To-Script for each file
```

### Watch Mode for Continuous Development
```javascript
// Start watch mode for active development
SN-Watch-Script({
  local_path: "/scripts/business_rules/validate_incident.js",
  script_sys_id: "abc123...",
  instance: "dev"
});

// Now you can:
// 1. Edit file in VSCode/WebStorm
// 2. Save (Cmd+S / Ctrl+S)
// 3. Changes auto-sync to ServiceNow in ~2 seconds
// 4. Test in ServiceNow immediately
// 5. Repeat!

// Stop watch mode when done (Ctrl+C or close terminal)
```

**Benefits:**
- Use local IDE features (IntelliSense, linting, debugging)
- Version control with Git
- Collaborative development (PRs, code reviews)
- Automated testing with local test frameworks
- Backup and disaster recovery

---

## 🗣️ Natural Language Interface

### Natural Language Search (NEW!)
Query ServiceNow using plain English instead of encoded queries:

```javascript
// Traditional way (encoded query)
SN-Query-Table({
  table_name: "incident",
  query: "active=true^priority=1^assigned_toISEMPTY",
  limit: 10
});

// Natural language way (easier!)
SN-NL-Search({
  table_name: "incident",
  query: "show me active high priority incidents that are unassigned",
  limit: 10
});
```

### Supported Query Patterns

**Field Comparisons:**
- "priority is 1" → `priority=1`
- "state equals In Progress" → `state=2`
- "assigned to John Smith" → `assigned_to=<sys_id>`

**Text Searches:**
- "description contains network" → `descriptionLIKEnetwork`
- "short description starts with Error" → `short_descriptionSTARTSWITHError`

**Logical Operators:**
- "priority is 1 AND state is New" → `priority=1^state=1`
- "priority is 1 OR priority is 2" → `priority=1^ORpriority=2`

**Empty/Not Empty:**
- "assigned to is empty" → `assigned_toISEMPTY`
- "resolution notes is not empty" → `resolution_notesISNOTEMPTY`

**Date Ranges:**
- "created today" → `sys_created_onONToday@javascript:gs.beginningOfToday()@javascript:gs.endOfToday()`
- "updated last 7 days" → `sys_updated_onONLast 7 days@javascript:gs.daysAgoStart(7)@javascript:gs.daysAgoEnd(0)`

**Ordering:**
- "sort by priority descending" → `ORDERBYDESCpriority`
- "order by created date" → `ORDERBYsys_created_on`

### When to Use NL vs Encoded Queries

**Use Natural Language When:**
- Exploratory queries (figuring out what you need)
- Complex multi-field searches
- Communicating requirements to stakeholders
- Prototyping queries before encoding

**Use Encoded Queries When:**
- Performance critical operations
- Programmatic/automated queries
- Complex OR conditions
- You already know the exact encoded query

### Examples

```javascript
// Example 1: Find urgent unassigned incidents
SN-NL-Search({
  table_name: "incident",
  query: "active incidents with urgency 1 where assigned to is empty",
  limit: 20,
  instance: "prod"
});

// Example 2: Recent high priority changes
SN-NL-Search({
  table_name: "change_request",
  query: "priority is critical and created in last 24 hours",
  fields: "number,short_description,state,assigned_to",
  instance: "prod"
});

// Example 3: Problem tickets with specific text
SN-NL-Search({
  table_name: "problem",
  query: "description contains database and state not equals Closed",
  limit: 10
});

// Example 4: Query builder (returns encoded query)
const encodedQuery = SN-NL-Query-Builder({
  natural_query: "show active incidents assigned to Network Team with priority 1 or 2"
});
// Returns: "active=true^assigned_to=<network_team_id>^priority=1^ORpriority=2"
```

---

## 📋 Standard Development Workflow

### 1. Set Application Context
```javascript
// For scoped app development
SN-Set-Current-Application({ app_sys_id: "your_app_id", instance: "dev" });
```

### 2. Set Update Set
```javascript
// Set current update set BEFORE any config changes
SN-Set-Update-Set({ update_set_sys_id: "your_update_set_id", instance: "dev" });
```

### 3. Create Configuration
```javascript
// All operations automatically captured in update set
SN-Create-Record({
  table_name: "sys_properties",
  data: {
    name: "x_custom.setting",
    value: "enabled"
  },
  instance: "dev"
});
```

### 4. Verify Capture
```javascript
// Verify records captured in correct update set
SN-Query-Table({
  table_name: "sys_update_xml",
  query: "update_set=<your_update_set_sys_id>",
  fields: "sys_id,type,name,sys_created_on",
  instance: "dev"
});
```

---

## ⚡ Best Practices

### MCP Operations
1. **MCP FIRST**: Always attempt MCP tools before background scripts
2. **Batch Operations**: Use parallel MCP calls (5-10+ in one message)
   - Example: 43 records moved in parallel successfully
3. **Field Selection**: Specify `fields` parameter to reduce payload
4. **Pagination**: Use `limit` and `offset` for large result sets
5. **Verification**: Always query results after operations
6. **Convenience Tools**: Use specialized tools for common operations (SN-Close-Incident, SN-Assign-Incident, etc.)
7. **Natural Language**: Use NL search for exploratory queries and prototyping
8. **Script Sync**: Develop scripts locally with version control and modern IDE features
9. **MCP Resources**: Use read-only resources for quick reference (schema, instances)

### Update Set Management
```javascript
// WRONG: Background script to move records
SN-Execute-Background-Script({ script: "..." }); // Can fail silently

// RIGHT: Direct REST API call
SN-Update-Record({
  table_name: "sys_update_xml",
  sys_id: "record_id",
  data: { update_set: "target_set_id" }
});

// BEST: Batch operation for multiple records
SN-Batch-Update({
  updates: [
    { table: "sys_update_xml", sys_id: "id1", data: { update_set: "target" }},
    { table: "sys_update_xml", sys_id: "id2", data: { update_set: "target" }}
  ]
});
```

### Discovery Flow
```javascript
// 1. List available tables
SN-List-Available-Tables({ category: "core_itsm" });

// 2. Get table schema
SN-Get-Table-Schema({ table_name: "change_request" });

// 3. Query records with filters
SN-Query-Table({
  table_name: "change_request",
  query: "state=1^priority=1",
  fields: "number,short_description,state",
  limit: 10
});
```

---

## 🏗️ Architecture

### Current Structure
```
src/
├── server.js                      # Express HTTP server (SSE transport)
├── stdio-server.js                # Stdio transport (Claude Desktop)
├── mcp-server-consolidated.js    # MCP tool registration (480+ tools)
├── servicenow-client.js           # ServiceNow REST API client
└── config-manager.js              # Multi-instance configuration

config/
└── servicenow-instances.json      # Multi-instance configuration

docs/
├── API_REFERENCE.md               # Complete API documentation
├── SETUP_GUIDE.md                 # Setup instructions
├── MULTI_INSTANCE_CONFIGURATION.md
├── INSTANCE_SWITCHING_GUIDE.md
├── 403_TROUBLESHOOTING.md
└── research/                      # Technical research & discoveries
```

### Key Features
- **Multi-Instance Support**: Single server manages multiple ServiceNow instances
- **34 Powerful MCP Tools**: Generic tools work on 160+ ServiceNow tables
- **Metadata-Driven**: Table schemas discovered dynamically
- **Session Management**: Separate MCP sessions per client
- **Background Script Automation**: Automated execution via sys_trigger

---

## 🎯 Complete Automation Example

```javascript
// FULLY AUTOMATED SCOPED APP DEVELOPMENT (Zero Manual Steps!)

// 1. Create scoped application
const app = SN-Create-Record({
  table_name: "sys_app",
  data: { name: "My Custom App", scope: "x_custom" },
  instance: "dev"
});

// 2. Set as current application
SN-Set-Current-Application({ app_sys_id: app.sys_id, instance: "dev" });

// 3. Create update set
const updateSet = SN-Create-Record({
  table_name: "sys_update_set",
  data: { name: "Feature Development", application: app.sys_id },
  instance: "dev"
});

// 4. Set as current update set
SN-Set-Update-Set({ update_set_sys_id: updateSet.sys_id, instance: "dev" });

// 5. Create business rule with script
const businessRule = SN-Create-Record({
  table_name: "sys_script",
  data: {
    name: "Validate Incident Priority",
    table: "incident",
    when: "before",
    script: "// Initial script"
  },
  instance: "dev"
});

// 6. Sync script locally for development
SN-Sync-Script-To-Local({
  script_sys_id: businessRule.sys_id,
  local_path: "/scripts/business_rules/validate_priority.js",
  instance: "dev"
});

// 7. Watch for local changes (continuous development)
SN-Watch-Script({
  local_path: "/scripts/business_rules/validate_priority.js",
  script_sys_id: businessRule.sys_id,
  instance: "dev"
});
// Edit locally, changes auto-sync to ServiceNow!

// 8. Find unassigned incidents using natural language
const incidents = SN-NL-Search({
  table_name: "incident",
  query: "active high priority incidents assigned to is empty",
  limit: 10,
  instance: "dev"
});

// 9. Assign incidents using convenience tool
for (const inc of incidents) {
  SN-Assign-Incident({
    sys_id: inc.sys_id,
    assigned_to: "admin",
    assignment_group: "Network Support",
    instance: "dev"
  });
}

// 10. Verify everything captured correctly
SN-Query-Table({
  table_name: "sys_update_xml",
  query: "update_set=" + updateSet.sys_id,
  fields: "sys_id,type,name",
  instance: "dev"
});

// ALL DONE!
// - Zero manual UI interaction
// - Version-controlled scripts
// - Natural language queries
// - Convenience operations
```

---

## ⚠️ Known Limitations

### Cannot Be Done via REST API
- ❌ **Flow Designer logic blocks**: Create flows in UI or use templates
- ❌ **Flow compilation**: Flows must be compiled in UI
- ⚠️ **UI Policy Actions linking**: Requires background script with setValue()

### Workarounds Available
- ✅ **Background scripts**: Fully automated via sys_trigger
- ✅ **Update set operations**: Fully automated API calls
- ✅ **Generic table operations**: Work on any custom table
- ✅ **Workflow creation**: Fully automated programmatic creation

**Complete Documentation:** `docs/research/FLOW_DESIGNER_LIMITATIONS.md`

---

## 🔍 Troubleshooting

### Permission Errors (403)
See `docs/403_TROUBLESHOOTING.md` for detailed solutions:
- System table permissions (sys_dictionary, sys_db_object)
- Required roles (personalize_dictionary, security_admin)
- ACL configuration

### Instance Connection Issues
```bash
# Verify instance configuration
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/instances

# Test ServiceNow connectivity
curl -u username:password https://your-instance.service-now.com/api/now/table/incident?sysparm_limit=1
```

### Background Script Debugging
```javascript
// Scripts log to ServiceNow System Logs
// Navigate to: System Logs → System Log → All
// Filter by: Source = "Script execution"
```

### Script Synchronization Issues

**File Permission Errors:**
```bash
# Error: EACCES: permission denied
# Solution: Ensure directory exists and is writable
mkdir -p /scripts/business_rules
chmod 755 /scripts/business_rules

# Or use absolute paths
SN-Sync-Script-To-Local({
  script_sys_id: "abc123",
  local_path: "/Users/username/servicenow/scripts/my_script.js"
});
```

**Watch Mode Not Detecting Changes:**
```bash
# Issue: Watch mode not syncing changes
# Causes:
# 1. File saved in different location
# 2. Watch process killed/stopped
# 3. Network connectivity issues

# Solution: Check watch process is running
ps aux | grep "watch-script"

# Restart watch mode
SN-Watch-Script({
  local_path: "/absolute/path/to/script.js",
  script_sys_id: "abc123",
  instance: "dev"
});
```

**Watch Mode Limitations:**
- Only watches single file (not directory)
- 2-second polling interval (not instant)
- Requires active network connection
- Stops when terminal closed

### Natural Language Query Issues

**Query Not Parsing as Expected:**
```javascript
// Problem: NL query returns wrong results
SN-NL-Search({
  table_name: "incident",
  query: "incidents from yesterday"  // Ambiguous!
});

// Solution: Be more specific
SN-NL-Search({
  table_name: "incident",
  query: "created yesterday"  // Clear field reference
});

// Or use encoded query for precision
SN-Query-Table({
  table_name: "incident",
  query: "sys_created_onONYesterday@javascript:gs.beginningOfYesterday()@javascript:gs.endOfYesterday()"
});
```

**Unsupported Patterns:**
```javascript
// NOT SUPPORTED: Complex aggregations
"count incidents by priority"  // Use GlideAggregate instead

// NOT SUPPORTED: Joins across tables
"incidents with related problems"  // Use multiple queries

// NOT SUPPORTED: Calculations
"incidents where age greater than 30 days"  // Use calculated fields
```

**Best Practice:**
Use `SN-NL-Query-Builder` to validate queries before execution:
```javascript
// Test query conversion first
const encoded = SN-NL-Query-Builder({
  natural_query: "active incidents assigned to Network Team"
});
console.log(encoded); // Verify encoded query is correct

// Then use in actual query
SN-Query-Table({
  table_name: "incident",
  query: encoded
});
```

---

## 📚 Documentation

### Essential Guides
- **[API Reference](docs/API_REFERENCE.md)** - Complete tool reference
- **[Setup Guide](docs/SETUP_GUIDE.md)** - Installation & configuration
- **[Multi-Instance Config](docs/MULTI_INSTANCE_CONFIGURATION.md)** - Multiple instances
- **[Instance Switching](docs/INSTANCE_SWITCHING_GUIDE.md)** - Routing requests

### Research & Discoveries
- **[Flow Designer Feasibility](docs/research/FLOW_DESIGNER_MCP_FEASIBILITY.md)** - Flow automation analysis
- **[Background Script Breakthrough](docs/research/UI_API_BREAKTHROUGH.md)** - Automated execution discovery
- **[Workflow Creation](docs/research/WORKFLOW_CREATION.md)** - Programmatic workflows

---

## 🚀 Quick Reference

### Most Common Operations
```javascript
// Set scope and update set (start of every session!)
SN-Set-Current-Application({ app_sys_id: "...", instance: "dev" });
SN-Set-Update-Set({ update_set_sys_id: "...", instance: "dev" });

// Query records
SN-Query-Table({ table_name: "incident", query: "active=true", limit: 10 });

// Create record
SN-Create-Record({ table_name: "sys_properties", data: {...} });

// Update record
SN-Update-Record({ table_name: "incident", sys_id: "...", data: {...} });

// Execute script (when needed)
SN-Execute-Background-Script({ script: "...", description: "..." });
```

### Tips for Success
1. **Always set application scope and update set FIRST**
2. **Use batch operations** - make 5-10+ MCP calls in one message
3. **Verify results** - query sys_update_xml after config changes
4. **Check schema** - SN-Get-Table-Schema before unfamiliar tables
5. **Monitor logs** - ServiceNow System Logs for background scripts
6. **Use convenience tools** - SN-Close-Incident, SN-Assign-Incident for common ITSM tasks
7. **NL for exploration** - Natural language queries great for prototyping
8. **Local development** - Sync scripts locally for version control and IDE features
9. **MCP resources** - Quick schema/instance lookups without API calls

---

## 📊 Statistics

- **MCP Tools:** 40+ powerful tools
- **Tables Supported:** 160+ ServiceNow tables (via generic tools)
- **Batch Operations:** 43+ parallel calls tested successfully
- **Script Execution:** ~1 second automated via sys_trigger
- **Instance Support:** Unlimited instances via config file
- **Generic CRUD:** Works on **any** ServiceNow table dynamically
- **Natural Language Patterns:** 15+ query patterns supported
- **Convenience Tools:** 10+ specialized ITSM operations
- **MCP Resources:** 3+ read-only resource types

---

## 🎓 Additional Resources

- **MCP Specification:** https://spec.modelcontextprotocol.io/
- **MCP SDK Docs:** https://www.npmjs.com/package/@modelcontextprotocol/sdk
- **ServiceNow REST API:** https://docs.servicenow.com/bundle/utah-api-reference/page/integrate/inbound-rest/concept/c_RESTAPI.html
- **MCP Inspector:** https://www.npmjs.com/package/@modelcontextprotocol/inspector
- **Project README:** `README.md`
- **Research Docs:** `docs/research/`