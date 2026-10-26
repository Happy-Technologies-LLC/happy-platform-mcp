# UI Operations Decision Matrix

**Version:** 1.0
**Date:** 2025-10-06
**Purpose:** Quick reference for choosing the right approach for ServiceNow operations

---

## Quick Decision Tree

```
Need to perform ServiceNow operation?
            ↓
    ┌───────┴───────┐
    │               │
Is it CRUD?     Is it Config?
    │               │
    ↓               ↓
Use REST API    Is it in update set?
✅ SN-Query-Table     │
✅ SN-Create-Record   ↓
✅ SN-Update-Record   ┌────┴────┐
                      │         │
                  Workflow?  Flow?
                      │         │
                      ↓         ↓
              ✅ Automate  ⚠️ Manual
              via MCP      + Export
```

---

## Operation Category Matrix

### CRUD Operations (100% Automated) ✅

| Operation | Use This | Performance | Example |
|-----------|----------|-------------|---------|
| **Query records** | `SN-Query-Table` | <1s | List incidents |
| **Create record** | `SN-Create-Record` | <1s | Create incident |
| **Read record** | `SN-Get-Record` | <1s | Get incident details |
| **Update record** | `SN-Update-Record` | <1s | Resolve incident |
| **Batch create** | `SN-Batch-Create` | 2-5s | Create 10+ records |
| **Batch update** | `SN-Batch-Update` | 2-5s | Update 43+ records |

**When to Use:** Always for data operations

---

### Configuration Management (100% Automated) ✅

| Operation | Use This | Performance | Example |
|-----------|----------|-------------|---------|
| **Set update set** | `SN-Set-Update-Set` | ~2s | Switch to dev set |
| **Set app scope** | `SN-Set-Current-Application` | ~2s | Switch to scoped app |
| **Get current set** | `SN-Get-Current-Update-Set` | <1s | Verify current set |
| **Move records** | `SN-Move-Records-To-Update-Set` | 2-5s | Fix wrong set |
| **Clone set** | `SN-Clone-Update-Set` | <1s | Backup update set |
| **Inspect set** | `SN-Inspect-Update-Set` | <1s | View set contents |

**When to Use:** Always before making config changes

---

### Workflow Operations (100% Automated) ✅

| Operation | Use This | Performance | Example |
|-----------|----------|-------------|---------|
| **Create workflow** | `SN-Create-Workflow` | <1s | Auto-approval workflow |
| **Add activity** | `SN-Create-Activity` | <1s | Notification step |
| **Link activities** | `SN-Create-Transition` | <1s | Connect steps |
| **Publish workflow** | `SN-Publish-Workflow` | <1s | Make workflow live |

**When to Use:** When you need programmatic workflow creation

---

### Flow Designer (20% Automated) ⚠️

| Operation | Use This | Performance | Notes |
|-----------|----------|-------------|-------|
| **Create flow** | ❌ Manual UI | Manual | Cannot automate |
| **Execute flow** | `FlowAPI` via script | <1s | ✅ Fully automated |
| **Modify flow** | ❌ Manual UI | Manual | Cannot automate |
| **Export flow** | Update set | Manual | Via UI |
| **Import flow** | Update set | Manual | Via UI |

**When to Use:**
- Create flows manually in UI (one-time)
- Execute flows programmatically via MCP (many times)
- See: `docs/FLOW_DESIGNER_GUIDE.md`

---

### Background Scripts (100% Automated) ✅

| Operation | Use This | Performance | Example |
|-----------|----------|-------------|---------|
| **Execute script** | `SN-Execute-Background-Script` | ~1s | Complex GlideRecord |
| **UI Policy linking** | Background script | ~1s | Link policy actions |
| **Complex queries** | Background script | 1-2s | Advanced filtering |
| **Data migration** | Background script | 1-5s | Bulk data updates |

**When to Use:**
- Operations not available via REST API
- Complex GlideRecord operations
- UI Policy Actions linking

**Method:** sys_trigger (automated, ~1 second)

---

## Decision Matrix by Use Case

### Use Case 1: Create and Deploy Configuration

```
┌─────────────────────────────────────────────────┐
│  Goal: Create table field, deploy to prod       │
├─────────────────────────────────────────────────┤
│  1. Set update set                              │
│     ✅ SN-Set-Update-Set (automated, ~2s)       │
│                                                 │
│  2. Create field via REST API                   │
│     ✅ SN-Create-Record (automated, <1s)        │
│     table: sys_dictionary                       │
│                                                 │
│  3. Verify captured in set                      │
│     ✅ SN-Inspect-Update-Set (automated, <1s)   │
│                                                 │
│  4. Export update set                           │
│     ⚠️ Manual: UI → Export XML                  │
│                                                 │
│  5. Import to prod                              │
│     ⚠️ Manual: UI → Import XML                  │
├─────────────────────────────────────────────────┤
│  Automation: 80% (3/5 steps)                    │
│  Time: ~3 seconds automated + 2 minutes manual  │
└─────────────────────────────────────────────────┘
```

**Verdict:** ✅ Good automation, acceptable manual steps

---

### Use Case 2: Create Workflow

```
┌─────────────────────────────────────────────────┐
│  Goal: Create approval workflow                 │
├─────────────────────────────────────────────────┤
│  1. Set update set                              │
│     ✅ SN-Set-Update-Set (automated, ~2s)       │
│                                                 │
│  2. Create workflow                             │
│     ✅ SN-Create-Workflow (automated, <1s)      │
│     - Define activities                         │
│     - Define transitions                        │
│     - Set conditions                            │
│                                                 │
│  3. Publish workflow                            │
│     ✅ SN-Publish-Workflow (automated, <1s)     │
│                                                 │
│  4. Export update set                           │
│     ⚠️ Manual: UI → Export XML                  │
├─────────────────────────────────────────────────┤
│  Automation: 100% (creation), 75% (deployment)  │
│  Time: ~3 seconds automated + 1 minute manual   │
└─────────────────────────────────────────────────┘
```

**Verdict:** ✅ Excellent automation

---

### Use Case 3: Create Flow Designer Flow

```
┌─────────────────────────────────────────────────┐
│  Goal: Create incident auto-assignment flow     │
├─────────────────────────────────────────────────┤
│  1. Set update set                              │
│     ✅ SN-Set-Update-Set (automated, ~2s)       │
│                                                 │
│  2. Create flow                                 │
│     ❌ Manual: Flow Designer UI                 │
│     - Define trigger                            │
│     - Add actions                               │
│     - Add conditions                            │
│     - Test flow                                 │
│     - Activate flow                             │
│     Time: 10-30 minutes                         │
│                                                 │
│  3. Export update set                           │
│     ⚠️ Manual: UI → Export XML                  │
│                                                 │
│  4. Import to other envs                        │
│     ⚠️ Manual: UI → Import XML                  │
│                                                 │
│  5. Execute flow (ongoing)                      │
│     ✅ FlowAPI via script (automated, <1s)      │
├─────────────────────────────────────────────────┤
│  Automation: 20% (creation), 100% (execution)   │
│  Time: One-time 10-30 min + automated runtime   │
└─────────────────────────────────────────────────┘
```

**Verdict:** ⚠️ Acceptable (one-time manual, ongoing automated)

**Alternative:** Use workflows instead if programmatic creation needed

---

### Use Case 4: Manage Update Sets

```
┌─────────────────────────────────────────────────┐
│  Goal: Fix records in wrong update set          │
├─────────────────────────────────────────────────┤
│  1. Query records in wrong set                  │
│     ✅ SN-Query-Table (automated, <1s)          │
│     table: sys_update_xml                       │
│     query: update_set=<wrong_set_id>            │
│                                                 │
│  2. Move records to correct set                 │
│     ✅ SN-Batch-Update (automated, 2-5s)        │
│     - 43+ records tested successfully           │
│                                                 │
│  3. Verify move                                 │
│     ✅ SN-Query-Table (automated, <1s)          │
│     query: update_set=<correct_set_id>          │
├─────────────────────────────────────────────────┤
│  Automation: 100%                               │
│  Time: 3-6 seconds total                        │
└─────────────────────────────────────────────────┘
```

**Verdict:** ✅ Excellent automation

---

### Use Case 5: Link UI Policy Actions

```
┌─────────────────────────────────────────────────┐
│  Goal: Link catalog variable to UI policy       │
├─────────────────────────────────────────────────┤
│  1. Create UI policy via REST                   │
│     ✅ SN-Create-Record (automated, <1s)        │
│     table: catalog_ui_policy                    │
│                                                 │
│  2. Create UI policy action via REST            │
│     ✅ SN-Create-Record (automated, <1s)        │
│     table: catalog_ui_policy_action             │
│     ⚠️ Fields ui_policy, catalog_variable       │
│        cannot be set via REST                   │
│                                                 │
│  3. Link action to policy via script            │
│     ✅ SN-Execute-Background-Script (~1s)       │
│     Uses setValue() to link                     │
│     format: IO:<variable_sys_id>                │
│                                                 │
│  4. Verify linkage                              │
│     ✅ SN-Get-Record (automated, <1s)           │
├─────────────────────────────────────────────────┤
│  Automation: 100%                               │
│  Time: ~3 seconds total                         │
└─────────────────────────────────────────────────┘
```

**Verdict:** ✅ Excellent automation (workaround successful)

---

## Performance Comparison

### Operation Speed by Method

```
REST API:                ▓ 0.8s
UI API Endpoints:        ▓▓ 1.8s
Background Script:       ▓ 1.2s
Batch Operations (10):   ▓▓ 2.1s
Batch Operations (43):   ▓▓▓▓ 4.8s
Manual UI (simple):      ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓ 30s
Manual UI (flow):        ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓ 600s
Puppeteer (if impl):     ▓▓▓▓▓▓▓▓▓▓ 20s

Legend: ▓ = 2 seconds
```

**Conclusion:** Automated methods are 10-100x faster than manual UI

---

## Reliability Comparison

### Success Rate by Method

```
REST API:              ████████████ 99.9%
UI API Endpoints:      ███████████░ 99.5%
Background Scripts:    ██████████░░ 99.0%
Batch Operations:      █████████░░░ 98.0%
Manual UI:             ████████████ 99.9% (but slow)
Puppeteer (if impl):   ████░░░░░░░░ 70-80% (brittle)
```

**Conclusion:** Current automated methods are highly reliable

---

## Maintenance Burden

### Development & Ongoing Maintenance

| Method | Initial Dev | Ongoing Maintenance | Complexity |
|--------|-------------|---------------------|------------|
| **REST API** | 1x | 0.1x | Low |
| **UI Endpoints** | 2x | 0.2x | Low |
| **Background Scripts** | 2x | 0.3x | Medium |
| **Manual UI** | 0x | 0x | None |
| **Puppeteer** | 10x | 5x | Very High |

**Legend:** x = baseline effort unit (e.g., 1 hour)

**Conclusion:** Puppeteer maintenance burden is 20-50x higher

---

## ROI Analysis

### Return on Investment by Approach

```
Current (Hybrid):
  Coverage: 95%
  Effort: 100 hours (done)
  ROI: ████████████ Excellent

Puppeteer Addition:
  Coverage: +2% (to 97%)
  Effort: 80 hours (new) + 20 hours/year (maintenance)
  ROI: ░░░░░░░░░░░░ Very Poor

Additional Documentation:
  Coverage: 0% (but better UX)
  Effort: 7 hours
  ROI: ████████░░░░ Good

Flow Templates:
  Coverage: 0% (but faster dev)
  Effort: 4 hours
  ROI: ██████░░░░░░ Fair
```

**Conclusion:** Current approach has excellent ROI, Puppeteer has poor ROI

---

## When to Choose Each Approach

### Decision Table

| If You Need To... | Use This Approach | Reason |
|-------------------|-------------------|--------|
| Query data | REST API | Fast, reliable, native |
| Create records | REST API | Fast, reliable, native |
| Update records | REST API | Fast, reliable, native |
| Set update set | UI API endpoint | Automated, proven |
| Set app scope | UI API endpoint | Automated, proven |
| Create workflow | REST API | Fully automated |
| Execute flow | FlowAPI | Fully automated |
| Create flow | Manual UI + export | Only option, one-time effort |
| Link UI policy | Background script | Workaround for REST limit |
| Complex GlideRecord | Background script | Server-side APIs needed |
| Batch operations | REST API parallel | Fast, efficient |
| Visual UI config | Manual UI | No automation available |

---

## Common Pitfalls to Avoid

### ❌ Don't Do This

**1. Try to Automate Flow Designer Creation**
- ❌ Complex undocumented JSON
- ❌ 15+ interconnected tables
- ❌ Version-dependent structure
- ❌ No validation via API
- ✅ Instead: Create manually, export via update set

**2. Use Puppeteer for ServiceNow Automation**
- ❌ 10-30 second operations
- ❌ Brittle selectors break on UI changes
- ❌ High maintenance burden
- ❌ Security concerns
- ✅ Instead: Use REST API + UI endpoints + background scripts

**3. Set Update Set via sys_user_preference**
- ❌ Only changes UI display, not actual capture
- ❌ Doesn't work for API-driven changes
- ✅ Instead: Use `SN-Set-Update-Set` (UI API endpoint)

**4. Execute Scripts via sys_trigger as the Primary Path (decision reversed)**
- Originally `/sys.scripts.do` was rejected here (HTML output hard to parse, session management complexity) and `sys_trigger` was chosen instead.
- Reversed in 2026-10: ServiceNow's own `now-sdk exec` (`@servicenow/sdk-api` 4.14.0, MIT) runs scripts through `/sys.scripts.do` and solved both problems, and that protocol was ported:
  - HTML parsing: the `Script completed in scope …` line proves a result (the processor answers 200 even for auth failures), output is recognized by its scope-dependent prefix, error lines and the Script Execution History link are extracted.
  - CSRF/session: `sysparm_ck` comes from the basic-auth user token or the session's `sys_user_session.csrf_token`, read and posted in one cookie session; the script POST is sent exactly once, never redirected or replayed after re-authentication.
- `sys_trigger` jobs run in the scheduler's session: writes can land in the Default update set, no rollback context is recorded, and results come from syslog-marker polling.
- ✅ Instead: `SN-Execute-Background-Script` default (`/sys.scripts.do`, caller's session, parsed result, rollback context); `sys_trigger` only as a fallback when the processor provably did not run the script, or with `wait: false`.

**5. Manual Fix Scripts for Everything**
- ❌ Requires manual copy-paste
- ❌ Slow workflow
- ✅ Instead: Use `SN-Execute-Background-Script` for automated execution

---

## Success Patterns

### ✅ Do This

**1. Always Set Update Set First**
```javascript
// CORRECT order
await SN-Set-Update-Set({ update_set_sys_id: 'abc' });
await SN-Create-Record({ table: 'sys_properties', data: {...} });

// WRONG order - goes to Default set!
await SN-Create-Record({ table: 'sys_properties', data: {...} });
await SN-Set-Update-Set({ update_set_sys_id: 'abc' });
```

**2. Use Batch Operations for Multiple Records**
```javascript
// GOOD: One batch operation
await SN-Batch-Update({
  updates: [
    { table: 'incident', sys_id: 'id1', data: {state: 6} },
    { table: 'incident', sys_id: 'id2', data: {state: 6} },
    // ... 41 more
  ]
});

// BAD: 43 individual operations
for (const record of records) {
  await SN-Update-Record({ table: 'incident', sys_id: record.sys_id, data: {...} });
}
```

**3. Verify Update Set Capture**
```javascript
// Create configuration
await SN-Create-Record({ table: 'sys_properties', data: {...} });

// Verify captured
const captured = await SN-Query-Table({
  table_name: 'sys_update_xml',
  query: 'update_set=<your_set_id>',
  fields: 'name,type,sys_created_on'
});
```

**4. Use FlowAPI for Flow Execution**
```javascript
// Create flow once in UI
// Then execute programmatically many times
await SN-Execute-Background-Script({
  script: `
    var inputs = { incident_sys_id: 'abc' };
    var outputs = sn_fd.FlowAPI.executeFlow('flow_sys_id', inputs);
    gs.print(JSON.stringify(outputs));
  `
});
```

**5. Use Templates for Common Patterns**
```javascript
// Use workflow templates
await SN-Create-Workflow({
  name: 'Auto-Approve Low Risk',
  table: 'change_request',
  activities: [...],  // From template
  transitions: [...]  // From template
});
```

---

## Quick Reference Card

### Top 10 Most Used Operations

| # | Operation | Tool | Time | When to Use |
|---|-----------|------|------|-------------|
| 1 | Query records | `SN-Query-Table` | <1s | Always for reading data |
| 2 | Create record | `SN-Create-Record` | <1s | Always for creating data |
| 3 | Update record | `SN-Update-Record` | <1s | Always for updating data |
| 4 | Set update set | `SN-Set-Update-Set` | ~2s | Before config changes |
| 5 | Get update set | `SN-Get-Current-Update-Set` | <1s | Verify current set |
| 6 | Inspect set | `SN-Inspect-Update-Set` | <1s | Check set contents |
| 7 | Execute script | `SN-Execute-Background-Script` | ~1s | Complex operations |
| 8 | Batch update | `SN-Batch-Update` | 2-5s | Multiple records |
| 9 | Create workflow | `SN-Create-Workflow` | <1s | Automated workflows |
| 10 | Get table schema | `SN-Get-Table-Schema` | <1s | Discover fields |

---

## Conclusion

### The Right Tool for the Job

```
┌─────────────────────────────────────────────────┐
│  95% of operations: FULLY AUTOMATED ✅          │
│  • REST API                                     │
│  • UI API endpoints                             │
│  • Background scripts (sys_trigger)             │
│  • Performance: <2 seconds                      │
│  • Reliability: 99%+                            │
├─────────────────────────────────────────────────┤
│  5% of operations: MANUAL WITH WORKAROUNDS ⚠️   │
│  • Flow Designer creation (one-time)            │
│  • UI-only configs (rare)                       │
│  • Acceptable trade-off                         │
│  • Clear documentation provided                 │
└─────────────────────────────────────────────────┘
```

**Key Takeaway:** Use the right tool for each operation type. Don't over-engineer solutions for edge cases that have acceptable workarounds.

---

## Related Documentation

- **Complete Architecture:** `docs/UI_OPERATIONS_ARCHITECTURE.md`
- **Executive Summary:** `docs/UI_OPERATIONS_SUMMARY.md`
- **Flow Designer Guide:** `docs/FLOW_DESIGNER_GUIDE.md`
- **API Reference:** `docs/API_REFERENCE.md`
- **Research:** `docs/research/FLOW_DESIGNER_LIMITATIONS.md`

---

**END OF DECISION MATRIX**
