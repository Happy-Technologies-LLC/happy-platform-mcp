//! The two 6.2.0 resources (`mcp-server-consolidated.js` `ListResourcesRequestSchema` /
//! `ReadResourceRequestSchema` handlers): `servicenow://instance` and
//! `servicenow://tables/all`. Registered in that order, which is the
//! `resources/list` order.

use std::sync::{Arc, OnceLock};

use hpm_core::config::{ConfigManager, PACKAGE_VERSION};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
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
            instance.insert("url".into(), Value::String(without_userinfo(url)));
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

/// TF-7: the instance URL without any `user:password@` part. A URL without
/// userinfo is echoed verbatim (6.2.0 text); one with userinfo is re-serialized
/// by `url::Url` with the userinfo removed. A value that does not parse as a URL
/// (or parses without a host, like `user:pass@host`) loses everything up to
/// the last `@` of its authority.
fn without_userinfo(raw: String) -> String {
    if let Ok(mut url) = reqwest::Url::parse(&raw)
        && url.has_host()
    {
        if url.username().is_empty() && url.password().is_none() {
            return raw;
        }
        if url.set_username("").is_ok() && url.set_password(None).is_ok() {
            return url.into();
        }
    }
    let (scheme, rest) = raw
        .split_once("://")
        .map_or(("", raw.as_str()), |(s, r)| (s, r));
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) if scheme.is_empty() => rest[at + 1..].to_owned(),
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => raw,
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
    fn instance_url_userinfo_is_never_echoed() {
        let url = |raw: &str| {
            info(&[("SERVICENOW_INSTANCE_URL", raw)], 1).payload()["instance_info"]["url"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(url("https://dev.example"), "https://dev.example");
        assert_eq!(
            url("https://admin:s3cret@dev.example"),
            "https://dev.example/"
        );
        assert_eq!(
            url("https://admin@dev.example/x?y"),
            "https://dev.example/x?y"
        );
        assert_eq!(
            url("https://:pw@dev.example:8443/"),
            "https://dev.example:8443/"
        );
        assert_eq!(url("admin:s3cret@dev.example/path"), "dev.example/path");
        assert_eq!(url("dev.example/a@b"), "dev.example/a@b");
    }

    #[test]
    fn unset_identity_leaves_the_keys_out() {
        let payload = info(&[], 6).payload();
        assert_eq!(payload["instance_info"], json!({}));
        assert_eq!(payload["capabilities"]["tools"], 6);
    }
}
