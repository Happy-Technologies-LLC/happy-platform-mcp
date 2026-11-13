//! Review W3 M4: the tools allowed `handler_validates_input` skip the
//! dispatcher's schema check, so their real handlers must refuse every
//! schema-invalid golden input themselves: 6.2.0's error text, no outbound
//! request, and (for the mutating tool) outcome `NotSent`. Composed like
//! `SingleUser`, with a recording audit sink and a counting transport.
//! One test per binary: the `AdmissionKey` mints once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hpm_core::client::ClientDeps;
use hpm_core::client::http::{HttpRequest, HttpResponse, HttpTransport, TransportError};
use hpm_core::config::{ConfigManager, Env};
use hpm_core::credential::provider::{InstanceClients, SingleUserCredentialProvider};
use hpm_core::keychain::MemorySecretStore;
use hpm_core::registry::InstanceRegistry;
use hpm_tools::dispatcher::{Dispatcher, Services};
use hpm_tools::families::{self, FamilyDeps};
use hpm_tools::registration::{HANDLER_VALIDATED_TOOLS, ToolRegistry};
use hpm_tools::schema::CompiledSchemaValidator;
use hpm_tools::single_user::{AllowAll, PassThrough, UnloadedClassification};
use hpm_types::BoxFut;
use hpm_types::audit::{
    AttemptRecord, AuditError, AuditEvent, AuditSink, PendingAttempt, SinkFailurePolicy,
    TerminalOutcome,
};
use hpm_types::context::{
    AdmissionInput, AdmissionKey, Principal, ProgressSink, ProtocolVersion, RequestContext,
    TrustedRoot,
};
use hpm_types::dispatch::DispatchOutcome;
use hpm_types::operation::ScriptOperandGuard;
use hpm_types::registry::RegistryStore;
use hpm_types::tool::{MutationOutcome, ToolError};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

struct NoProgress;
impl ProgressSink for NoProgress {
    fn emit(&self, _: f64, _: Option<f64>, _: Option<String>) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
}

/// Counts every request; none may be sent.
struct Counting(Arc<AtomicUsize>);
impl HttpTransport for Counting {
    fn send(&self, _: HttpRequest) -> BoxFut<'_, Result<HttpResponse, TransportError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(TransportError {
                message: "no network in this test".into(),
                code: None,
            })
        })
    }
}

#[derive(Default)]
struct Outcomes(Mutex<Vec<MutationOutcome>>);
impl AuditSink for Outcomes {
    fn begin_attempt<'a>(
        &'a self,
        a: AttemptRecord,
    ) -> BoxFut<'a, Result<PendingAttempt, AuditError>> {
        Box::pin(async move { Ok(PendingAttempt { call_id: a.call_id }) })
    }
    fn complete<'a>(
        &'a self,
        _: &'a PendingAttempt,
        t: TerminalOutcome,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        self.0.lock().expect("lock").push(t.outcome);
        Box::pin(async { Ok(()) })
    }
    fn record<'a>(&'a self, _: AuditEvent) -> BoxFut<'a, Result<(), AuditError>> {
        Box::pin(async { Ok(()) })
    }
    fn failure_policy(&self) -> SinkFailurePolicy {
        SinkFailurePolicy::FailClosedForMutations
    }
    fn audits_reads(&self) -> bool {
        false
    }
}

/// What the protocol adapter admits for a stdio call without an `instance`
/// argument: the registry's default instance.
fn ctx(config: &ConfigManager) -> RequestContext {
    let instance = config
        .snapshot_for(&Principal::LocalOperator)
        .resolve_requested(None)
        .expect("default instance");
    TrustedRoot::single_user_stdio()
        .admit(
            AdmissionInput {
                mcp_request_id: "1".into(),
                protocol_version: ProtocolVersion::new("2025-06-18"),
                client_capabilities: Value::Null,
                client_name: "test".into(),
                instance,
                instance_miss: None,
                cancel: CancellationToken::new(),
                deadline: Instant::now() + Duration::from_secs(60),
                progress: Arc::new(NoProgress),
            },
            None,
        )
        .expect("stdio admission")
}

fn golden(path: &str) -> Value {
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden/tools")
        .join(path);
    serde_json::from_str(&std::fs::read_to_string(file).expect("golden")).expect("golden json")
}

#[tokio::test]
async fn flagged_tools_refuse_schema_invalid_input_without_sending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry_path = dir.path().join("instances.json");
    let env = Env::from_pairs(&[
        ("HOME", dir.path().join("home").to_str().expect("utf-8")),
        (
            "XDG_CONFIG_HOME",
            dir.path().join("xdg").to_str().expect("utf-8"),
        ),
        ("HAPPY_CONFIG_PATH", registry_path.to_str().expect("utf-8")),
        ("SERVICENOW_TOKEN_STORE", "file"),
        ("SERVICENOW_INSTANCE_URL", "https://dev.example.invalid"),
        ("SERVICENOW_USERNAME", "golden.user"),
        ("SERVICENOW_PASSWORD", "not-a-secret"),
    ]);
    let config = Arc::new(ConfigManager::new(
        Arc::new(InstanceRegistry::at_paths(
            registry_path.clone(),
            registry_path.clone(),
            env.clone(),
        )),
        env,
    ));
    let sent = Arc::new(AtomicUsize::new(0));
    let mut deps = ClientDeps::production(Arc::new(MemorySecretStore::default())).expect("deps");
    deps.transport = Arc::new(Counting(sent.clone()));
    let store = deps.credential_store.clone();
    let clients = Arc::new(InstanceClients::from_config(config.clone(), deps));
    let family_deps = FamilyDeps::new(clients.clone(), config.clone(), store);
    let mut registry = ToolRegistry::new().expect("registry");
    families::register_all(&mut registry, &family_deps).expect("families");
    let flagged: Vec<&str> = registry
        .tools()
        .iter()
        .filter(|t| t.handler_validates_input)
        .map(|t| t.name)
        .collect();
    assert_eq!(flagged.len(), HANDLER_VALIDATED_TOOLS.len(), "{flagged:?}");
    let audit = Arc::new(Outcomes::default());
    let services = Services {
        validator: Arc::new(CompiledSchemaValidator::new(registry.tools()).expect("schemas")),
        classification: Arc::new(UnloadedClassification),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: Arc::new(SingleUserCredentialProvider::new(clients)),
        audit: audit.clone(),
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
        usage: None,
    };
    let key = AdmissionKey::mint().expect("first mint in this process");
    let d = Dispatcher::new(registry, services, key);

    for case in [
        "atf/SN-Get-ATF-Results__bad-result-type.json",
        "instances/SN-Register-Instance__secret-field-rejected.json",
        "instances/SN-Register-Instance__insecure-url-rejected.json",
    ] {
        let g = golden(case);
        let step = &g["steps"][0];
        let params = &step["request"]["params"];
        let tool = params["name"].as_str().expect("tool name");
        assert!(HANDLER_VALIDATED_TOOLS.contains(&tool), "{case}");
        let want = step["response"]["result"]["content"][0]["text"]
            .as_str()
            .expect("golden text");
        let before = audit.0.lock().expect("lock").len();
        let got = match d
            .call_tool(&ctx(&config), tool, params["arguments"].clone())
            .await
        {
            Ok(DispatchOutcome::Rendered(r)) => {
                assert_eq!(r.is_error, Some(true), "{case}");
                r.text
            }
            Ok(other) => panic!("{case}: {other:?}"),
            Err(ToolError::Text(t)) => t,
            // The protocol adapter renders `Message` as `Error: <message>`.
            Err(ToolError::Message(m)) => format!("Error: {m}"),
            Err(e) => panic!("{case}: generic refusal {e:?}"),
        };
        assert_eq!(got, want, "{case}");
        assert_eq!(sent.load(Ordering::SeqCst), 0, "{case}: outbound request");
        let outcomes = audit.0.lock().expect("lock");
        // SN-Register-Instance mutates: its attempt must finish `NotSent`.
        // SN-Get-ATF-Results is a read: no attempt is recorded.
        let want_outcomes: &[MutationOutcome] = if tool == "SN-Register-Instance" {
            &[MutationOutcome::NotSent]
        } else {
            &[]
        };
        assert_eq!(&outcomes[before..], want_outcomes, "{case}");
    }
}
