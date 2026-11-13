//! G1/G5 and the Q1 re-capture (owner-approved, plan W3 §6 item 1).
//!
//! `fixtures/tools-list-7.0.json` is the 7.0 `tools/list`: the real registry
//! (every family's `register`) in registration order, each tool's name,
//! description, input schema and the D-05 `annotation_hints` the registry attaches
//! centrally. The registry must render to it byte for byte. Regenerate after an
//! intended change with `HPM_WRITE_TOOLS_LIST=1 cargo test -p hpm-tools --test
//! tools_list`, then update `tools70.fixture.sha256` in `fixtures/compat-manifest.json`.
//!
//! Against the immutable 6.2.0 list (`fixtures/tools-list-6.2.0.json`, name-sorted):
//! the same 60 names, and with `annotations` removed every tool is byte-equal,
//! except the entries in [`DEVIATIONS_FROM_6_2_0`]. Registration order is the
//! 6.2.0 source order (compat-manifest `tools.items`).

use std::path::PathBuf;
use std::sync::Arc;

use hpm_core::client::ClientDeps;
use hpm_core::config::ConfigManager;
use hpm_core::credential::provider::InstanceClients;
use hpm_core::keychain::LazySecretStore;
use hpm_tools::families::{self, FamilyDeps};
use hpm_tools::registration::ToolRegistry;
use hpm_tools::tool_order::{DOCS_ONLY_ORDER, TOOLS_LIST_ORDER};
use serde_json::{Map, Value, json};

/// 7.0 corrections to 6.2.0 tool text (owner decisions): tool, JSON pointer into
/// the tool, 6.2.0 value (`None`: added in 7.0), decision.
const DEVIATIONS_FROM_6_2_0: &[(&str, &str, Option<&str>, &str)] = &[
    (
        "SN-Batch-Create",
        "/inputSchema/properties/transaction/description",
        Some("All-or-nothing transaction (default: true)"),
        "owner decision 11: 6.2.0 never rolls back; the 7.0 text says a mid-batch \
         failure can leave earlier records created and that the error reports them",
    ),
    (
        "SN-Execute-Background-Script",
        "/description",
        Some(EXECUTE_BACKGROUND_SCRIPT_DESC_620),
        "owner decision P4-2: the sys_trigger fallback is opt-in; the 7.0 text no \
         longer says a refused processor run falls back to sys_trigger",
    ),
    (
        "SN-Execute-Background-Script",
        "/inputSchema/properties/allow_trigger_fallback",
        None,
        "owner decision P4-2: per-call consent to the sys_trigger fallback",
    ),
];

const EXECUTE_BACKGROUND_SCRIPT_DESC_620: &str = r#"Executes server-side JavaScript the way Scripts - Background does. Use for complex GlideRecord operations the CRUD tools cannot express. By default (wait: true) the script is POSTed once to the instance's /sys.scripts.do processor as the authenticated user, and the response is parsed into success/failure, output lines, errors, the scope it ran in, and links to its Script Execution History and rollback context. Requires the admin role. A rollback context is recorded by default (record_for_rollback), so the run's record changes can be rolled back from that context. The processor runs in the authenticated user's own session, so that user's current update set is expected to capture tracked configuration writes, and `scope` picks the application scope. The script request is never retried: a timeout, 5xx, or dropped connection is reported as an unknown outcome (check Script Execution History; do not re-run blindly). Only when the processor provably did not run the script (HTTP 401/403/404, a login page, or a connection error before sending) does it fall back to a sys_trigger job, and the response says so. The sys_trigger path (that fallback, or wait: false) runs in the scheduler's own session, not the caller's: tracked configuration writes may be captured into the Default update set (verify sys_update_xml and move rows with SN-Move-Records-To-Update-Set), GlideUpdateSet calls in it cannot change the update set other tools or the UI use (use SN-Set-Update-Set), and no rollback context is recorded. As a fallback it polls syslog for a correlation marker and reports "completed", "failed", or "timeout" (not a success); wait: false returns only trigger scheduling metadata immediately (see issue #40)."#;

fn fixture_path(name: &str) -> PathBuf {
    [env!("CARGO_MANIFEST_DIR"), "..", "..", "fixtures", name]
        .iter()
        .collect()
}

fn read_json(name: &str) -> Value {
    let raw = std::fs::read_to_string(fixture_path(name)).expect("fixture");
    serde_json::from_str(&raw).expect("fixture parses")
}

fn registry() -> ToolRegistry {
    let config = Arc::new(ConfigManager::from_process_env());
    let deps = ClientDeps::production(Arc::new(LazySecretStore::os())).expect("client deps");
    let store = deps.credential_store.clone();
    let clients = Arc::new(InstanceClients::from_config(config.clone(), deps));
    let family_deps = FamilyDeps::new(clients, config, store);
    let mut registry = ToolRegistry::new().expect("registry");
    // Registration rejects any tool whose declared classes differ from its D-05 row
    // (`RegistrationError::ClassificationMismatch`), so success here is the exit-gate
    // check "all 60 tools match D-05 classification at registration".
    families::register_all(&mut registry, &family_deps).expect("register");
    registry
}

/// The 7.0 `tools/list` entries in MCP shape (`annotations` only when the D-05
/// row has hints, with only the hints it sets).
fn render(registry: &ToolRegistry) -> Value {
    let tools: Vec<Value> = registry
        .tools()
        .iter()
        .map(|t| {
            let mut tool = Map::new();
            tool.insert("name".into(), json!(t.name));
            tool.insert("description".into(), json!(t.description));
            let schema: Value = serde_json::from_str(t.input_schema_json).expect("schema");
            tool.insert("inputSchema".into(), schema);
            if let Some(h) = &t.annotations {
                let mut a = Map::new();
                for (k, v) in [
                    ("readOnlyHint", h.read_only_hint),
                    ("destructiveHint", h.destructive_hint),
                    ("idempotentHint", h.idempotent_hint),
                    ("openWorldHint", h.open_world_hint),
                ] {
                    if let Some(v) = v {
                        a.insert(k.into(), json!(v));
                    }
                }
                tool.insert("annotations".into(), Value::Object(a));
            }
            Value::Object(tool)
        })
        .collect();
    json!({ "tools": tools })
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("serialize");
    s.push('\n');
    s
}

#[test]
fn registry_renders_the_7_0_tools_list_byte_for_byte() {
    let mut registry = registry();
    assert_eq!(registry.tools().len(), 60);
    let listed: Vec<&str> = registry.tools().iter().map(|t| t.name).collect();
    assert_eq!(listed, TOOLS_LIST_ORDER, "registration order == 6.2.0");
    let manifest = read_json("compat-manifest.json");
    let source_order: Vec<&str> = manifest["tools"]["items"]
        .as_array()
        .expect("compat-manifest tools.items")
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert_eq!(listed, source_order, "compat-manifest 6.2.0 source order");

    let got = pretty(&render(&registry));
    let path = fixture_path("tools-list-7.0.json");
    if std::env::var_os("HPM_WRITE_TOOLS_LIST").is_some() {
        std::fs::write(&path, &got).expect("write tools-list-7.0.json");
    }
    let want = std::fs::read_to_string(&path).expect("tools-list-7.0.json");
    assert!(
        got == want,
        "registry differs from fixtures/tools-list-7.0.json; regenerate (see module doc) if intended"
    );

    registry.retain_docs_only();
    let docs: Vec<&str> = registry.tools().iter().map(|t| t.name).collect();
    assert_eq!(docs, DOCS_ONLY_ORDER);
}

#[test]
fn tools_list_7_0_is_6_2_0_plus_hints_and_owner_corrections() {
    let old = read_json("tools-list-6.2.0.json");
    let new = read_json("tools-list-7.0.json");
    let old = old["tools"].as_array().expect("6.2.0 tools");
    let new = new["tools"].as_array().expect("7.0 tools");
    assert_eq!(old.len(), new.len());
    let mut hinted = 0;
    for tool in new {
        let name = tool["name"].as_str().expect("name");
        let mut tool = tool.clone();
        if tool
            .as_object_mut()
            .expect("object")
            .remove("annotations")
            .is_some()
        {
            hinted += 1;
        }
        let mut want = old
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} not in 6.2.0"))
            .clone();
        for (_, pointer, was, _) in DEVIATIONS_FROM_6_2_0.iter().filter(|d| d.0 == name) {
            let now = tool.pointer(pointer).expect("deviation pointer in 7.0");
            match was {
                Some(was) => {
                    let slot = want
                        .pointer_mut(pointer)
                        .expect("deviation pointer in 6.2.0");
                    assert_eq!(slot, was, "{name}{pointer}: recorded 6.2.0 value");
                    assert_ne!(now, was, "{name}{pointer}: 7.0 must differ");
                    *slot = now.clone();
                }
                // Added in 7.0: absent from 6.2.0, inserted into its parent.
                None => {
                    assert!(
                        want.pointer(pointer).is_none(),
                        "{name}{pointer}: new in 7.0"
                    );
                    let (parent, key) = pointer.rsplit_once('/').expect("pointer");
                    want.pointer_mut(parent)
                        .and_then(Value::as_object_mut)
                        .expect("deviation parent in 6.2.0")
                        .insert(key.to_owned(), now.clone());
                }
            }
        }
        // The 6.2.0 file is stored with recursively sorted keys (compat-manifest
        // `hashing`), so compare values; that is byte equality after canonical sort.
        // Wire key order is covered by the surface replay of the 6.2.0 capture.
        assert_eq!(tool, want, "{name}");
    }
    assert!(hinted > 0, "no D-05 hints in the 7.0 list");
    let batch = new
        .iter()
        .find(|t| t["name"] == "SN-Batch-Create")
        .expect("SN-Batch-Create");
    let text = batch["inputSchema"]["properties"]["transaction"]["description"]
        .as_str()
        .expect("transaction description");
    assert!(!text.to_lowercase().starts_with("all-or-nothing"), "{text}");
    assert!(
        text.contains("already") || text.contains("earlier"),
        "{text}"
    );
}
