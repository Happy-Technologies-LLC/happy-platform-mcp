//! 6.2.0 descriptions and input schemas (from `fixtures/tools-list-6.2.0.json`).

/// (name, description, input schema JSON).
pub(super) const TOOLS: &[(&str, &str, &str)] = &[
    (
        "SN-List-Incidents",
        "List Incident records with filtering and pagination",
        r#"{"properties":{"fields":{"description":"Comma-separated list of fields to return (optional)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"limit":{"default":25,"description":"Maximum number of records to return (default: 25)","type":"number"},"offset":{"description":"Number of records to skip for pagination (optional)","type":"number"},"order_by":{"description":"Field to sort by (e.g., \"created_on\" or \"-priority\" for descending) (optional)","type":"string"},"priority":{"description":"Filter by priority (1-5) (optional)","type":"number"},"query":{"description":"ServiceNow encoded query string (e.g., \"state=1^priority=1\") (optional)","type":"string"},"state":{"description":"Filter by state (e.g., \"New\", \"In Progress\", \"Resolved\") (optional)","type":"string"}},"required":[],"type":"object"}"#,
    ),
    (
        "SN-Create-Incident",
        "Create a new Incident",
        r#"{"properties":{"assigned_to":{"description":"assigned to (optional)","type":"string"},"assignment_group":{"description":"assignment group (optional)","type":"string"},"caller_id":{"description":"caller id (optional)","type":"string"},"category":{"description":"category (optional)","type":"string"},"description":{"description":"description (optional)","type":"string"},"impact":{"description":"impact (optional)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"priority":{"description":"priority (optional)","type":"string"},"short_description":{"description":"short description (required)","type":"string"},"state":{"description":"state (optional)","type":"string"},"subcategory":{"description":"subcategory (optional)","type":"string"},"sys_created_by":{"description":"sys created by (optional)","type":"string"},"sys_created_on":{"description":"sys created on (optional)","type":"string"},"sys_updated_by":{"description":"sys updated by (optional)","type":"string"},"sys_updated_on":{"description":"sys updated on (optional)","type":"string"},"urgency":{"description":"urgency (optional)","type":"string"},"work_notes":{"description":"work notes (optional)","type":"string"}},"required":["short_description"],"type":"object"}"#,
    ),
    (
        "SN-Get-Incident",
        "Get a Incident by ID",
        r#"{"properties":{"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"sys_id":{"description":"System ID","type":"string"}},"required":["sys_id"],"type":"object"}"#,
    ),
    (
        "SN-List-SysUsers",
        "List Sys User records with filtering and pagination",
        r#"{"properties":{"fields":{"description":"Comma-separated list of fields to return (optional)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"limit":{"default":25,"description":"Maximum number of records to return (default: 25)","type":"number"},"offset":{"description":"Number of records to skip for pagination (optional)","type":"number"},"order_by":{"description":"Field to sort by (e.g., \"created_on\" or \"-priority\" for descending) (optional)","type":"string"},"query":{"description":"ServiceNow encoded query string (e.g., \"state=1^priority=1\") (optional)","type":"string"}},"required":[],"type":"object"}"#,
    ),
    (
        "SN-List-CmdbCis",
        "List Cmdb Ci records with filtering and pagination",
        r#"{"properties":{"fields":{"description":"Comma-separated list of fields to return (optional)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"limit":{"default":25,"description":"Maximum number of records to return (default: 25)","type":"number"},"offset":{"description":"Number of records to skip for pagination (optional)","type":"number"},"order_by":{"description":"Field to sort by (e.g., \"created_on\" or \"-priority\" for descending) (optional)","type":"string"},"query":{"description":"ServiceNow encoded query string (e.g., \"state=1^priority=1\") (optional)","type":"string"}},"required":[],"type":"object"}"#,
    ),
    (
        "SN-List-SysUserGroups",
        "List Sys User Group records with filtering and pagination",
        r#"{"properties":{"fields":{"description":"Comma-separated list of fields to return (optional)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"limit":{"default":25,"description":"Maximum number of records to return (default: 25)","type":"number"},"offset":{"description":"Number of records to skip for pagination (optional)","type":"number"},"order_by":{"description":"Field to sort by (e.g., \"created_on\" or \"-priority\" for descending) (optional)","type":"string"},"query":{"description":"ServiceNow encoded query string (e.g., \"state=1^priority=1\") (optional)","type":"string"}},"required":[],"type":"object"}"#,
    ),
    (
        "SN-List-ChangeRequests",
        "List Change Request records with filtering and pagination",
        r#"{"properties":{"fields":{"description":"Comma-separated list of fields to return (optional)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"limit":{"default":25,"description":"Maximum number of records to return (default: 25)","type":"number"},"offset":{"description":"Number of records to skip for pagination (optional)","type":"number"},"order_by":{"description":"Field to sort by (e.g., \"created_on\" or \"-priority\" for descending) (optional)","type":"string"},"query":{"description":"ServiceNow encoded query string (e.g., \"state=1^priority=1\") (optional)","type":"string"},"state":{"description":"Filter by change state (optional)","type":"string"},"type":{"description":"Filter by change type (e.g., \"Normal\", \"Emergency\") (optional)","type":"string"}},"required":[],"type":"object"}"#,
    ),
    (
        "SN-List-Problems",
        "List Problem records with filtering and pagination",
        r#"{"properties":{"fields":{"description":"Comma-separated list of fields to return (optional)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"limit":{"default":25,"description":"Maximum number of records to return (default: 25)","type":"number"},"offset":{"description":"Number of records to skip for pagination (optional)","type":"number"},"order_by":{"description":"Field to sort by (e.g., \"created_on\" or \"-priority\" for descending) (optional)","type":"string"},"query":{"description":"ServiceNow encoded query string (e.g., \"state=1^priority=1\") (optional)","type":"string"}},"required":[],"type":"object"}"#,
    ),
    (
        "SN-Validate-Configuration",
        "Validate catalog item configuration including variables, UI policies, and business rules",
        r#"{"properties":{"catalog_item":{"description":"Catalog item sys_id (required)","type":"string"},"checks":{"description":"Validation checks to perform","properties":{"ui_policies":{"properties":{"check_actions_linked":{"type":"boolean"},"check_conditions":{"type":"boolean"},"check_variables_exist":{"type":"boolean"}},"type":"object"},"variables":{"properties":{"check_choices":{"type":"boolean"},"check_linked":{"type":"boolean"},"check_mandatory":{"type":"boolean"},"check_types":{"type":"boolean"}},"type":"object"}},"type":"object"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"}},"required":["catalog_item"],"type":"object"}"#,
    ),
    (
        "SN-Add-Comment",
        "Add a comment to an incident. Accepts incident number for better UX.",
        r#"{"properties":{"comment":{"description":"Comment text to add (required)","type":"string"},"incident_number":{"description":"Incident number (e.g., \"INC0012345\") (required)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"}},"required":["incident_number","comment"],"type":"object"}"#,
    ),
    (
        "SN-Add-Work-Notes",
        "Add work notes to an incident. Accepts incident number for better UX.",
        r#"{"properties":{"incident_number":{"description":"Incident number (e.g., \"INC0012345\") (required)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"work_notes":{"description":"Work notes text to add (required)","type":"string"}},"required":["incident_number","work_notes"],"type":"object"}"#,
    ),
    (
        "SN-Assign-Incident",
        "Assign an incident to a user and/or group. Resolves user names automatically.",
        r#"{"properties":{"assigned_to":{"description":"User name or sys_id to assign to (required)","type":"string"},"assignment_group":{"description":"Assignment group name or sys_id (optional)","type":"string"},"incident_number":{"description":"Incident number (e.g., \"INC0012345\") (required)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"}},"required":["incident_number","assigned_to"],"type":"object"}"#,
    ),
    (
        "SN-Resolve-Incident",
        "Resolve an incident with resolution notes and code.",
        r#"{"properties":{"incident_number":{"description":"Incident number (e.g., \"INC0012345\") (required)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"resolution_code":{"description":"Resolution code (e.g., \"Solved (Permanently)\", \"Solved (Work Around)\") (optional)","type":"string"},"resolution_notes":{"description":"Resolution notes describing the fix (required)","type":"string"}},"required":["incident_number","resolution_notes"],"type":"object"}"#,
    ),
    (
        "SN-Close-Incident",
        "Close an incident with close notes and code.",
        r#"{"properties":{"close_code":{"description":"Close code (e.g., \"Solved (Permanently)\", \"Solved (Work Around)\") (optional)","type":"string"},"close_notes":{"description":"Close notes (required)","type":"string"},"incident_number":{"description":"Incident number (e.g., \"INC0012345\") (required)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"}},"required":["incident_number","close_notes"],"type":"object"}"#,
    ),
    (
        "SN-Add-Change-Comment",
        "Add a comment to a change request. Accepts change number for better UX.",
        r#"{"properties":{"change_number":{"description":"Change request number (e.g., \"CHG0012345\") (required)","type":"string"},"comment":{"description":"Comment text to add (required)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"}},"required":["change_number","comment"],"type":"object"}"#,
    ),
    (
        "SN-Assign-Change",
        "Assign a change request to a user and/or group.",
        r#"{"properties":{"assigned_to":{"description":"User name or sys_id to assign to (required)","type":"string"},"assignment_group":{"description":"Assignment group name or sys_id (optional)","type":"string"},"change_number":{"description":"Change request number (e.g., \"CHG0012345\") (required)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"}},"required":["change_number","assigned_to"],"type":"object"}"#,
    ),
    (
        "SN-Approve-Change",
        "Approve a change request.",
        r#"{"properties":{"approval_comments":{"description":"Comments for the approval (optional)","type":"string"},"change_number":{"description":"Change request number (e.g., \"CHG0012345\") (required)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"}},"required":["change_number"],"type":"object"}"#,
    ),
    (
        "SN-Add-Problem-Comment",
        "Add a comment to a problem. Accepts problem number for better UX.",
        r#"{"properties":{"comment":{"description":"Comment text to add (required)","type":"string"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"problem_number":{"description":"Problem number (e.g., \"PRB0012345\") (required)","type":"string"}},"required":["problem_number","comment"],"type":"object"}"#,
    ),
    (
        "SN-Close-Problem",
        "Close a problem with resolution information.",
        r#"{"properties":{"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"problem_number":{"description":"Problem number (e.g., \"PRB0012345\") (required)","type":"string"},"resolution_code":{"description":"Resolution code (optional)","type":"string"},"resolution_notes":{"description":"Resolution notes (required)","type":"string"}},"required":["problem_number","resolution_notes"],"type":"object"}"#,
    ),
    (
        "SN-Catalog-Get-Item",
        "Retrieve full form context for a Service Catalog item or record producer. Returns item metadata, all variables with human-readable type names, choice options, variable sets, UI policy conditions/effects, and client script names. Use this before SN-Catalog-Submit to understand exactly which fields to fill and what values are valid.",
        r#"{"properties":{"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"sys_id":{"description":"Catalog item or record producer sys_id (required)","type":"string"}},"required":["sys_id"],"type":"object"}"#,
    ),
    (
        "SN-Catalog-Search-Items",
        "Search or browse Service Catalog items and record producers. Use this to discover available forms before calling SN-Catalog-Get-Item to get full form details.",
        r#"{"properties":{"active":{"default":true,"description":"Filter by active status (default: true)","type":"boolean"},"category":{"description":"Category sys_id to filter results (optional)","type":"string"},"include_producers":{"default":true,"description":"Also search record producers in addition to catalog items (default: true)","type":"boolean"},"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"keyword":{"description":"Keyword to match against item name or short description (optional)","type":"string"},"limit":{"default":25,"description":"Maximum number of results to return (default: 25)","type":"integer"}},"type":"object"}"#,
    ),
    (
        "SN-Catalog-Submit",
        "Submit a Service Catalog form (catalog item or record producer). Provide the catalog item sys_id and a map of variable names to values. Use SN-Catalog-Get-Item first to discover required variables, their types, and valid choices. Returns the resulting request/task numbers.",
        r#"{"properties":{"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"quantity":{"default":1,"description":"Quantity to order (default: 1)","type":"integer"},"requested_for":{"description":"User sys_id or username to request on behalf of (optional, defaults to authenticated user)","type":"string"},"sys_id":{"description":"Catalog item or record producer sys_id (required)","type":"string"},"variables":{"additionalProperties":true,"description":"Map of variable name to value, e.g. { \"requested_for\": \"jsmith\", \"justification\": \"Need access for project X\" } (required)","type":"object"}},"required":["sys_id","variables"],"type":"object"}"#,
    ),
    (
        "SN-Catalog-Get-Categories",
        "List Service Catalog categories so you can browse available form families. Use the returned sys_ids with SN-Catalog-Search-Items to filter forms by category.",
        r#"{"properties":{"instance":{"description":"Configured ServiceNow instance name. Optional; uses the current instance when omitted.","type":"string"},"limit":{"default":50,"description":"Maximum number of results to return (default: 50)","type":"integer"},"query":{"description":"Encoded query to filter categories (optional, e.g. \"active=true\")","type":"string"}},"type":"object"}"#,
    ),
];
