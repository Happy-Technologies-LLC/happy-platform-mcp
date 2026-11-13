//! `servicenow://instance` reports the tools this server lists (6.2.0
//! `(await listTools()).tools.length`): in `--docs-only` mode that is the six docs
//! tools, not everything the families registered. The count is read per request,
//! after docs-only filtering. One test per binary: the `AdmissionKey` mints once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hpm_cli::SingleUser;
use hpm_core::client::ClientDeps;
use hpm_core::config::{ConfigManager, Env};
use hpm_core::keychain::MemorySecretStore;
use hpm_core::registry::InstanceRegistry;
use hpm_tools::tool_order::DOCS_ONLY_ORDER;
use hpm_types::BoxFut;
use hpm_types::context::{
    AdmissionInput, AdmissionKey, ProgressSink, ProtocolVersion, TrustedRoot,
};
use tokio_util::sync::CancellationToken;

struct NoProgress;

impl ProgressSink for NoProgress {
    fn emit(&self, _: f64, _: Option<f64>, _: Option<String>) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
}

#[tokio::test]
async fn docs_only_instance_resource_counts_the_docs_tools() {
    let env = Env::from_pairs(&[]);
    let registry = Arc::new(InstanceRegistry::new(env.clone()));
    let config = Arc::new(ConfigManager::new(registry, env));
    let deps = ClientDeps::production(Arc::new(MemorySecretStore::default())).expect("deps");
    let key = AdmissionKey::mint().expect("first mint in this process");
    let dispatcher = SingleUser::new(config, deps)
        .docs_only(true)
        .dispatcher(key)
        .expect("docs-only dispatcher");
    let ctx = TrustedRoot::single_user_stdio()
        .admit(
            AdmissionInput {
                mcp_request_id: "1".into(),
                protocol_version: ProtocolVersion::new("2025-06-18"),
                client_capabilities: serde_json::Value::Null,
                client_name: "test".into(),
                instance: None,
                instance_miss: None,
                cancel: CancellationToken::new(),
                deadline: Instant::now() + Duration::from_secs(60),
                progress: Arc::new(NoProgress),
            },
            None,
        )
        .expect("stdio admission");
    let resource = dispatcher
        .resources()
        .iter()
        .find(|r| r.uri == "servicenow://instance")
        .expect("instance resource listed");
    let payload = dispatcher
        .read_resource(&ctx, resource)
        .await
        .expect("resource read");
    // What the dispatcher lists (every docs-only tool the registered families provide),
    // not what the families registered before filtering.
    let listed: Vec<&str> = dispatcher.tools().iter().map(|t| t.name).collect();
    assert!(!listed.is_empty() && listed.iter().all(|n| DOCS_ONLY_ORDER.contains(n)));
    assert_eq!(payload["capabilities"]["tools"], listed.len(), "{payload}");
}
