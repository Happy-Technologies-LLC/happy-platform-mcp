# ServiceNow MCP Tools - Quick Reference Card

## 🚀 New Convenience Tools (10)

### Incident Tools
```javascript
// Add comment (customer-visible)
SN-Add-Comment({ incident_number: "INC0012345", comment: "..." })

// Add work notes (internal)
SN-Add-Work-Notes({ incident_number: "INC0012345", work_notes: "..." })

// Assign to user/group (auto-resolves names)
SN-Assign-Incident({ incident_number: "INC0012345", assigned_to: "John Smith", assignment_group: "IT Support" })

// Resolve (state → 6)
SN-Resolve-Incident({ incident_number: "INC0012345", resolution_notes: "...", resolution_code: "Solved (Permanently)" })

// Close (state → 7)
SN-Close-Incident({ incident_number: "INC0012345", close_notes: "...", close_code: "Solved (Permanently)" })
```

### Change Request Tools
```javascript
// Add comment
SN-Add-Change-Comment({ change_number: "CHG0012345", comment: "..." })

// Assign to user/group
SN-Assign-Change({ change_number: "CHG0012345", assigned_to: "Jane Doe", assignment_group: "Change Advisory Board" })

// Approve
SN-Approve-Change({ change_number: "CHG0012345", approval_comments: "..." })
```

### Problem Tools
```javascript
// Add comment
SN-Add-Problem-Comment({ problem_number: "PRB0012345", comment: "..." })

// Close (state → 3)
SN-Close-Problem({ problem_number: "PRB0012345", resolution_notes: "...", resolution_code: "..." })
```

---

## 📋 Core CRUD Tools (6)

```javascript
// Query any table
SN-Query-Table({ table_name: "incident", query: "state=1^priority=1", limit: 25 })

// Create record
SN-Create-Record({ table_name: "incident", data: { short_description: "..." } })

// Get single record
SN-Get-Record({ table_name: "incident", sys_id: "...", fields: "number,state,priority" })

// Update record
SN-Update-Record({ table_name: "incident", sys_id: "...", data: { state: 6 } })

// Get table schema
SN-Get-Table-Schema({ table_name: "incident" })

// List available tables
SN-List-Available-Tables({ category: "core_itsm" })
```

---

## 🔄 Update Set Management (6)

```javascript
// Set current update set (AUTOMATED!)
SN-Set-Update-Set({ update_set_sys_id: "..." })

// Get current update set
SN-Get-Current-Update-Set()

// List update sets
SN-List-Update-Sets({ query: "state=in progress", limit: 25 })

// Move records to update set
SN-Move-Records-To-Update-Set({ update_set_id: "...", source_update_set: "Default" })

// Clone update set
SN-Clone-Update-Set({ source_update_set_id: "...", new_name: "..." })

// Inspect update set contents
SN-Inspect-Update-Set({ update_set: "...", show_components: true })
```

---

## 🔧 Workflow Tools (4)

```javascript
// Create complete workflow
SN-Create-Workflow({
  name: "My Workflow",
  table: "incident",
  activities: [...],
  transitions: [...],
  publish: true
})

// Create activity
SN-Create-Activity({ workflow_version_sys_id: "...", name: "...", script: "..." })

// Create transition
SN-Create-Transition({ from_activity_sys_id: "...", to_activity_sys_id: "..." })

// Publish workflow
SN-Publish-Workflow({ version_sys_id: "...", start_activity_sys_id: "..." })
```

---

## 📦 Batch Operations (2)

```javascript
// Batch create with references
SN-Batch-Create({
  operations: [
    { table: "sc_cat_item", data: {...}, save_as: "item" },
    { table: "item_option_new", data: { cat_item: "${item}" } }
  ],
  transaction: true
})

// Batch update
SN-Batch-Update({
  updates: [
    { table: "incident", sys_id: "...", data: { state: 6 } },
    { table: "incident", sys_id: "...", data: { state: 6 } }
  ],
  stop_on_error: false
})
```

---

## 🎯 Specialized Tools (16)

### ITSM Convenience
```javascript
SN-List-Incidents({ state: "1", priority: 1, limit: 25 })
SN-Create-Incident({ short_description: "..." })
SN-Get-Incident({ sys_id: "..." })
SN-List-ChangeRequests({ state: "Pending Approval" })
SN-List-Problems({ state: "Open" })
```

### User/Group Management
```javascript
SN-List-SysUsers({ query: "active=true", limit: 25 })
SN-List-SysUserGroups({ query: "active=true" })
SN-List-CmdbCis({ query: "..." })
```

### Application Scope
```javascript
SN-Set-Current-Application({ app_sys_id: "..." })
```

### Schema Discovery
```javascript
SN-Discover-Table-Schema({
  table_name: "incident",
  include_type_codes: true,
  include_relationships: true
})

SN-Explain-Field({ table: "incident", field: "priority", include_examples: true })
```

### Configuration Validation
```javascript
SN-Validate-Configuration({
  catalog_item: "...",
  checks: {
    variables: { check_linked: true, check_types: true },
    ui_policies: { check_conditions: true }
  }
})
```

### Script Execution
```javascript
// Execute background script (AUTOMATED via sys_trigger!)
SN-Execute-Background-Script({
  script: "gs.info('Hello World');",
  description: "Test script",
  execution_method: "trigger"  // Default, most reliable
})

// Create fix script (manual execution)
SN-Create-Fix-Script({
  script_name: "fix_data",
  script_content: "...",
  auto_delete: false
})
```

---

## 🌐 Multi-Instance Support

All tools support the `instance` parameter:

```javascript
// Route to specific instance
SN-Query-Table({
  table_name: "incident",
  query: "state=1",
  instance: "dev"  // Routes to dev instance
})

// Default instance (from config)
SN-Query-Table({
  table_name: "incident",
  query: "state=1"
  // Uses default instance
})

// Switch instance
SN-Set-Instance({ instance_name: "dev" })

// Get current instance
SN-Get-Current-Instance()
```

---

## 🛒 Service Catalog AI-Submission (4)

```javascript
// 1. Browse categories to find a form family
SN-Catalog-Get-Categories({ limit: 50 })

// 2. Search for the right form by keyword or category
SN-Catalog-Search-Items({ keyword: "VPN access", active: true, include_producers: true })

// 3. Read full form context (field types, choices, mandatory rules, UI policies)
SN-Catalog-Get-Item({ sys_id: "<catalog_item_sys_id>" })

// 4. Submit with a variable map
SN-Catalog-Submit({
  sys_id: "<catalog_item_sys_id>",
  variables: {
    requested_for: "jsmith",
    justification: "Need VPN for project X",
    start_date: "2026-05-01"
  },
  quantity: 1
})
```

---

## 📊 Tool Count Summary

| Category | Tools | Notes |
|----------|-------|-------|
| **Core CRUD** | 6 | Generic table operations |
| **Update Sets** | 6 | Automated set management |
| **Workflows** | 4 | Complete workflow creation |
| **Batch Ops** | 2 | Multi-record operations |
| **ITSM Convenience** | 10 | Incident/Change/Problem |
| **Service Catalog** | 4 | **NEW!** Browse/inspect/submit catalog forms |
| **Specialized** | 16 | Users, schema, validation, scripts |
| **TOTAL** | **48** | Up from 44 |

---

## 🎯 Common Workflows

### Complete Incident Resolution
```javascript
// 1. List open incidents
SN-List-Incidents({ state: "1", limit: 10 })

// 2. Add work notes
SN-Add-Work-Notes({ incident_number: "INC0012345", work_notes: "Investigating..." })

// 3. Assign to engineer
SN-Assign-Incident({ incident_number: "INC0012345", assigned_to: "John Smith" })

// 4. Add customer comment
SN-Add-Comment({ incident_number: "INC0012345", comment: "Working on fix" })

// 5. Resolve
SN-Resolve-Incident({
  incident_number: "INC0012345",
  resolution_notes: "Fixed",
  resolution_code: "Solved (Permanently)"
})

// 6. Close
SN-Close-Incident({
  incident_number: "INC0012345",
  close_notes: "Confirmed",
  close_code: "Solved (Permanently)"
})
```

### Change Management
```javascript
// 1. Assign for review
SN-Assign-Change({ change_number: "CHG0012345", assigned_to: "Jane Doe" })

// 2. Add review comment
SN-Add-Change-Comment({ change_number: "CHG0012345", comment: "Review complete" })

// 3. Approve
SN-Approve-Change({ change_number: "CHG0012345", approval_comments: "Approved" })
```

### Update Set Workflow
```javascript
// 1. Create update set
SN-Create-Record({
  table_name: "sys_update_set",
  data: { name: "My Feature", description: "..." }
})

// 2. Set as current (AUTOMATED!)
SN-Set-Update-Set({ update_set_sys_id: "..." })

// 3. Make changes (auto-captured)
SN-Create-Record({ table_name: "sys_properties", data: {...} })

// 4. Verify capture
SN-Inspect-Update-Set({ update_set: "..." })

// 5. Clone for backup
SN-Clone-Update-Set({ source_update_set_id: "...", new_name: "Backup" })
```

### Catalog Form Submission
```javascript
// 1. Find the right form
SN-Catalog-Search-Items({ keyword: "laptop", active: true })

// 2. Read full form context
SN-Catalog-Get-Item({ sys_id: "<catalog_item_sys_id>" })
// → Review variables[].name, variables[].type, variables[].choices

// 3. Submit
SN-Catalog-Submit({
  sys_id: "<catalog_item_sys_id>",
  variables: {
    requested_for: "jsmith",
    justification: "New hire onboarding"
  }
})
```

---

## ⚡ Performance Tips

### Use Batch Operations
```javascript
// ❌ Slow: 10 individual calls
for (let i = 0; i < 10; i++) {
  SN-Update-Record({ table_name: "incident", sys_id: ids[i], data: { state: 6 } })
}

// ✅ Fast: 1 batch call
SN-Batch-Update({
  updates: ids.map(id => ({ table: "incident", sys_id: id, data: { state: 6 } }))
})
```

### Limit Fields Returned
```javascript
// ❌ Returns all fields
SN-Query-Table({ table_name: "incident", limit: 100 })

// ✅ Returns only needed fields
SN-Query-Table({
  table_name: "incident",
  fields: "number,state,priority,short_description",
  limit: 100
})
```

### Use Convenience Tools
```javascript
// ❌ 3 tool calls
SN-Query-Table({ table_name: "incident", query: "number=INC0012345" })
// Extract sys_id
SN-Update-Record({ table_name: "incident", sys_id: "...", data: { comments: "..." } })

// ✅ 1 tool call (66% faster)
SN-Add-Comment({ incident_number: "INC0012345", comment: "..." })
```

---

## 🔍 Troubleshooting

### Record Not Found
```
Error: Incident INC0012345 not found
→ Verify incident number is correct
→ Check instance (dev vs prod)
→ Ensure you have permissions
```

### User/Group Not Found
```
Error: User "John Smith" not found
→ Verify name exactly matches ServiceNow
→ Try using sys_id instead
→ Use SN-List-SysUsers to search
```

### Permission Denied
```
Error: 403 Forbidden
→ Check user roles (admin, itil, etc.)
→ Verify table ACLs
→ See docs/403_TROUBLESHOOTING.md
```

---

## 📚 Documentation

- **Complete Guide:** `/docs/CONVENIENCE_TOOLS.md`
- **API Reference:** `/docs/API_REFERENCE.md`
- **Setup Guide:** `/docs/SETUP_GUIDE.md`
- **Quick Summary:** `/CONVENIENCE_TOOLS_SUMMARY.md`
- **This Card:** `/QUICK_REFERENCE.md`

---

**Version:** 2.2
**Total Tools:** 48
**Last Updated:** 2026-04-19
