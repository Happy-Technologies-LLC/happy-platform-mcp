//! A transport failure's outcome through a REST tool family (R18): a
//! connect-phase failure (the request provably never left) finishes `NotSent`,
//! while a failure once connected (timeout, reset) stays `Unknown`: the server
//! may have acted on it. `SN-Create-Record` end to end through the dispatcher,
//! with a recording audit sink and a scripted transport.
//! One test per binary: the `AdmissionKey` mints once.

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
use hpm_tools::registration::ToolRegistry;
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
use hpm_types::tool::MutationOutcome;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

struct NoProgress;
impl ProgressSink for NoProgress {
    fn emit(&self, _: f64, _: Option<f64>, _: Option<String>) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
}

/// Fails every request with the scripted transport error.
struct Failing(Mutex<TransportError>);
impl HttpTransport for Failing {
    fn send(&self, _: HttpRequest) -> BoxFut<'_, Result<HttpResponse, TransportError>> {
        let error = self.0.lock().expect("lock").clone();
        Box::pin(async move { Err(error) })
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

#[tokio::test]
async fn connect_phase_failure_is_not_sent_and_post_connect_failure_is_unknown() {
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
    let transport = Arc::new(Failing(Mutex::new(TransportError::new("ETIMEDOUT", "x"))));
    let mut deps = ClientDeps::production(Arc::new(MemorySecretStore::default())).expect("deps");
    deps.transport = transport.clone();
    let store = deps.credential_store.clone();
    let clients = Arc::new(InstanceClients::from_config(config.clone(), deps));
    let family_deps = FamilyDeps::new(clients.clone(), config.clone(), store);
    let mut registry = ToolRegistry::new().expect("registry");
    families::register_all(&mut registry, &family_deps).expect("families");
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
    let args = serde_json::json!({
        "table_name": "incident",
        "data": {"short_description": "outcome probe"},
    });

    let cases = [
        (
            "connect-phase timeout: nothing was sent",
            TransportError::connect("ETIMEDOUT", "ETIMEDOUT: error sending request (Connect)"),
            MutationOutcome::NotSent,
        ),
        (
            "connection refused: nothing was sent",
            TransportError::connect("ECONNREFUSED", "ECONNREFUSED: connection refused"),
            MutationOutcome::NotSent,
        ),
        (
            "timeout once connected: the request may have been acted on",
            TransportError::new("ETIMEDOUT", "ETIMEDOUT: operation timed out"),
            MutationOutcome::Unknown,
        ),
        (
            "reset mid-body: the request may have been acted on",
            TransportError::new("ECONNRESET", "ECONNRESET: connection reset"),
            MutationOutcome::Unknown,
        ),
        (
            "no code at all: the request may have been acted on",
            TransportError {
                code: None,
                message: "socket hang up".into(),
                connect: false,
            },
            MutationOutcome::Unknown,
        ),
    ];
    for (name, failure, want) in cases {
        *transport.0.lock().expect("lock") = failure;
        let before = audit.0.lock().expect("lock").len();
        let got = d
            .call_tool(&ctx(&config), "SN-Create-Record", args.clone())
            .await;
        assert!(
            !matches!(got, Ok(DispatchOutcome::Rendered(ref r)) if r.is_error != Some(true)),
            "{name}: must fail"
        );
        let outcomes = audit.0.lock().expect("lock");
        assert_eq!(&outcomes[before..], &[want], "{name}");
    }
}
