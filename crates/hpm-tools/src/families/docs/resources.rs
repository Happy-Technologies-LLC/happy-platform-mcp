//! The two 6.2.0 resources (`mcp-server-consolidated.js` `ListResourcesRequestSchema` /
//! `ReadResourceRequestSchema` handlers): `servicenow://instance` and
//! `servicenow://tables/all`. Registered in that order, which is the
//! `resources/list` order.

use std::sync::{Arc, OnceLock};

use hpm_core::config::PACKAGE_VERSION;
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::tool::{OutputClass, ResourceDescriptor, ResourceHandler, ToolError};
use serde_json::{Value, json};

use crate::families::FamilyDeps;
use crate::registration::{RegistrationError, ToolRegistry};
use crate::table_metadata;

struct InstanceInfo {
    /// 6.2.0 `(await listTools()).tools.length`: the tools this server lists. Set by
    /// the composition root after docs-only filtering, so it is read per request.
    tool_count: Arc<OnceLock<usize>>,
}

impl InstanceInfo {
    /// GHSA-cmrc-rrgj-4v7m (CWE-200): `instance_info` is always empty. 6.2.0 to 7.2.2
    /// echoed `SERVICENOW_INSTANCE_URL` and `SERVICENOW_USERNAME` here; a resource
    /// reader (any MCP client, or any principal the policy lets read it) learned
    /// the instance address and the account name. The key stays, as `{}`, so a
    /// client that reads `instance_info` keeps working.
    fn payload(&self) -> Value {
        json!({
            "server_info": {
                "name": "Happy MCP Server (Consolidated)",
                "version": PACKAGE_VERSION,
                "description": "Consolidated ServiceNow integration with metadata-driven schema lookups",
            },
            "instance_info": {},
            "capabilities": {
                "total_tables": table_metadata::tables().len(),
                "operations": ["create", "read", "update", "query", "schema_lookup"],
                "tools": self.tool_count.get().copied().unwrap_or(0),
            },
        })
    }
}

impl ResourceHandler for InstanceInfo {
    fn read<'a>(&'a self, _: &'a RequestContext) -> BoxFut<'a, Result<Value, ToolError>> {
        Box::pin(async move { Ok(self.payload()) })
    }
}

struct AllTables;

impl ResourceHandler for AllTables {
    fn read<'a>(&'a self, _: &'a RequestContext) -> BoxFut<'a, Result<Value, ToolError>> {
        Box::pin(async move { Ok(Value::Object(table_metadata::tables().clone())) })
    }
}

pub(super) fn register(
    registry: &mut ToolRegistry,
    deps: &FamilyDeps,
) -> Result<(), RegistrationError> {
    registry.register_resource(ResourceDescriptor::new(
        "servicenow://instance",
        "ServiceNow Instance Info",
        "Information about the connected ServiceNow instance",
        "application/json",
        OutputClass::Schema,
        Arc::new(InstanceInfo {
            tool_count: deps.tool_count.clone(),
        }),
    ))?;
    registry.register_resource(ResourceDescriptor::new(
        "servicenow://tables/all",
        "All ServiceNow Tables",
        "Complete list of available ServiceNow tables with metadata",
        "application/json",
        OutputClass::Schema,
        Arc::new(AllTables),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(tool_count: usize) -> InstanceInfo {
        let count = Arc::new(OnceLock::new());
        count.set(tool_count).unwrap();
        InstanceInfo { tool_count: count }
    }

    #[test]
    fn instance_info_lists_server_identity_and_counts() {
        let payload = info(60).payload();
        assert_eq!(payload["server_info"]["version"], PACKAGE_VERSION);
        assert_eq!(payload["capabilities"]["tools"], 60);
        assert_eq!(
            payload["capabilities"]["total_tables"],
            table_metadata::tables().len()
        );
    }

    /// GHSA-cmrc-rrgj-4v7m: the resource never carries the instance address or the
    /// account name. `crates/hpm-cli/tests/instance_resource_disclosure.rs` sets both
    /// in the environment and reads the resource through the dispatcher.
    #[test]
    fn instance_info_carries_no_url_or_username() {
        let payload = info(6).payload();
        assert_eq!(payload["instance_info"], json!({}));
        let text = payload.to_string();
        assert!(!text.contains("\"url\"") && !text.contains("\"username\""));
    }
}
