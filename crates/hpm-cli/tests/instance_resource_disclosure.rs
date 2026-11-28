//! GHSA-cmrc-rrgj-4v7m (CWE-200): `servicenow://instance` must not disclose the
//! instance address or the account name, whatever the environment holds. 6.2.0 to
//! 7.2.2 echoed `SERVICENOW_INSTANCE_URL` and `SERVICENOW_USERNAME` in
//! `instance_info`. One test per binary: the `AdmissionKey` mints once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hpm_cli::SingleUser;
use hpm_core::client::ClientDeps;
use hpm_core::config::{ConfigManager, Env};
use hpm_core::keychain::MemorySecretStore;
use hpm_core::registry::InstanceRegistry;
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
async fn instance_resource_does_not_disclose_url_or_username() {
    let env = Env::from_pairs(&[
        (
            "SERVICENOW_INSTANCE_URL",
            "https://tenant-secret-9f3a.service-now.com",
        ),
        ("SERVICENOW_USERNAME", "svc-secret-account"),
    ]);
    let registry = Arc::new(InstanceRegistry::new(env.clone()));
    let config = Arc::new(ConfigManager::new(registry, env));
    let deps = ClientDeps::production(Arc::new(MemorySecretStore::default())).expect("deps");
    let key = AdmissionKey::mint().expect("first mint in this process");
    let dispatcher = SingleUser::new(config, deps)
        .dispatcher(key)
        .expect("dispatcher");
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
    assert_eq!(payload["instance_info"], serde_json::json!({}), "{payload}");
    let text = payload.to_string();
    for secret in [
        "tenant-secret-9f3a",
        "service-now.com",
        "svc-secret-account",
    ] {
        assert!(
            !text.contains(secret),
            "servicenow://instance leaked an instance identity value"
        );
    }
    assert!(
        payload["capabilities"]["tools"].as_u64().unwrap() > 0,
        "{payload}"
    );
}
