//! G1/G5: the registered families list exactly the 6.2.0 tools, in 6.2.0 source
//! order (compat-manifest `tools.items`, [`TOOLS_LIST_ORDER`]), with byte-equal
//! descriptions and input schemas compared **by name** against the name-sorted
//! fixture (compat-manifest: "the fixture file is name-sorted so it is compared by
//! name"). `title`/`annotations` (D4, 7.0 additions) are ignored. Ignored until
//! every lane has landed.

use std::path::PathBuf;
use std::sync::Arc;

use hpm_core::client::ClientDeps;
use hpm_core::config::ConfigManager;
use hpm_core::credential::provider::InstanceClients;
use hpm_core::keychain::LazySecretStore;
use hpm_tools::families::{self, FamilyDeps};
use hpm_tools::registration::ToolRegistry;
use hpm_tools::tool_order::{DOCS_ONLY_ORDER, TOOLS_LIST_ORDER};
use serde_json::Value;

fn fixture() -> Value {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "fixtures",
        "tools-list-6.2.0.json",
    ]
    .iter()
    .collect();
    let raw = std::fs::read_to_string(&path).expect("tools-list fixture");
    serde_json::from_str(&raw).expect("fixture parses")
}

fn registry() -> ToolRegistry {
    let config = Arc::new(ConfigManager::from_process_env());
    let deps = ClientDeps::production(Arc::new(LazySecretStore::os())).expect("client deps");
    let store = deps.credential_store.clone();
    let clients = Arc::new(InstanceClients::from_config(config.clone(), deps));
    let family_deps = FamilyDeps::new(clients, config, store);
    let mut registry = ToolRegistry::new().expect("registry");
    families::register_all(&mut registry, &family_deps).expect("register");
    registry
}

#[test]
#[ignore = "until every W3 lane has registered its family"]
fn families_list_the_6_2_0_tools_in_source_order() {
    let mut registry = registry();
    let listed: Vec<&str> = registry.tools().iter().map(|t| t.name).collect();
    assert_eq!(listed, TOOLS_LIST_ORDER);

    let expected = fixture();
    let expected = expected["tools"].as_array().expect("tools array");
    assert_eq!(expected.len(), registry.tools().len());
    for tool in registry.tools() {
        let want = expected
            .iter()
            .find(|t| t["name"].as_str() == Some(tool.name))
            .unwrap_or_else(|| panic!("{} not in fixture", tool.name));
        assert_eq!(
            Some(tool.description),
            want["description"].as_str(),
            "{}",
            tool.name
        );
        let schema: Value = serde_json::from_str(tool.input_schema_json).expect("schema");
        assert_eq!(
            serde_json::to_string(&schema).expect("serialize"),
            serde_json::to_string(&want["inputSchema"]).expect("serialize"),
            "{}",
            tool.name
        );
    }

    registry.retain_docs_only();
    let docs: Vec<&str> = registry.tools().iter().map(|t| t.name).collect();
    assert_eq!(docs, DOCS_ONLY_ORDER);
}
