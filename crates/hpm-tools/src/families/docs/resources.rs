//! The two 6.2.0 resources (`mcp-server-consolidated.js` `ListResourcesRequestSchema` /
//! `ReadResourceRequestSchema` handlers): `servicenow://instance` and
//! `servicenow://tables/all`. Registered in that order, which is the
//! `resources/list` order.

use std::sync::{Arc, OnceLock};

use hpm_core::config::{ConfigManager, PACKAGE_VERSION};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::credential::CredentialNeed;
use hpm_types::tool::{OutputClass, ResourceDescriptor, ResourceHandler, ToolError};
use serde_json::{Map, Value, json};

use crate::families::FamilyDeps;
use crate::registration::{RegistrationError, ToolRegistry};
use crate::table_metadata;

struct InstanceInfo {
    config: Arc<ConfigManager>,
    /// 6.2.0 `(await listTools()).tools.length`: the tools this server lists. Set by
    /// the composition root after docs-only filtering, so it is read per request.
    tool_count: Arc<OnceLock<usize>>,
}

impl InstanceInfo {
    fn payload(&self) -> Value {
        // `JSON.stringify` drops `undefined`: an unset variable leaves the key out.
        let env = self.config.env();
        let mut instance = Map::new();
        if let Some(url) = env.get("SERVICENOW_INSTANCE_URL") {
            instance.insert("url".into(), Value::String(url));
        }
        if let Some(username) = env.get("SERVICENOW_USERNAME") {
            instance.insert("username".into(), Value::String(username));
        }
        json!({
            "server_info": {
                "name": "Happy MCP Server (Consolidated)",
                "version": PACKAGE_VERSION,
                "description": "Consolidated ServiceNow integration with metadata-driven schema lookups",
            },
            "instance_info": instance,
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
        CredentialNeed::None,
        Arc::new(InstanceInfo {
            config: deps.config.clone(),
            tool_count: deps.tool_count.clone(),
        }),
    ))?;
    registry.register_resource(ResourceDescriptor::new(
        "servicenow://tables/all",
        "All ServiceNow Tables",
        "Complete list of available ServiceNow tables with metadata",
        "application/json",
        OutputClass::Schema,
        CredentialNeed::None,
        Arc::new(AllTables),
    ))
}

#[cfg(test)]
mod tests {
    use hpm_core::config::Env;
    use hpm_core::registry::InstanceRegistry;

    use super::*;

    fn info(pairs: &[(&str, &str)], tool_count: usize) -> InstanceInfo {
        let env = Env::from_pairs(pairs);
        let registry = Arc::new(InstanceRegistry::new(env.clone()));
        let count = Arc::new(OnceLock::new());
        count.set(tool_count).unwrap();
        InstanceInfo {
            config: Arc::new(ConfigManager::new(registry, env)),
            tool_count: count,
        }
    }

    #[test]
    fn instance_info_lists_env_identity_and_counts() {
        let payload = info(
            &[
                ("SERVICENOW_INSTANCE_URL", "https://dev.example"),
                ("SERVICENOW_USERNAME", "golden.user"),
            ],
            60,
        )
        .payload();
        assert_eq!(payload["server_info"]["version"], PACKAGE_VERSION);
        assert_eq!(payload["instance_info"]["url"], "https://dev.example");
        assert_eq!(payload["instance_info"]["username"], "golden.user");
        assert_eq!(payload["capabilities"]["tools"], 60);
        assert_eq!(
            payload["capabilities"]["total_tables"],
            table_metadata::tables().len()
        );
    }

    #[test]
    fn unset_identity_leaves_the_keys_out() {
        let payload = info(&[], 6).payload();
        assert_eq!(payload["instance_info"], json!({}));
        assert_eq!(payload["capabilities"]["tools"], 6);
    }
}
