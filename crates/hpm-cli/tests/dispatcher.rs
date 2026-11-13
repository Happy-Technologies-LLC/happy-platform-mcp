//! Dispatcher pipeline tests. They live in the composition root, which owns the
//! `trusted-root` feature (review H1). The `AdmissionKey` mints once per process,
//! so every test shares one key.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use hpm_tools::dispatcher::{Dispatcher, RequiredPropertiesValidator, Services};
use hpm_tools::registration::{RegistrationError, ToolRegistry};
use hpm_tools::single_user::{AllowAll, LocalOnlyCredentials, PassThrough};
use hpm_types::BoxFut;
use hpm_types::audit::{
    AttemptRecord, AuditError, AuditEvent, AuditSink, PendingAttempt, SinkFailurePolicy,
    TerminalOutcome,
};
use hpm_types::context::{
    AdmissionInput, AdmissionKey, ProgressSink, ProtocolVersion, RequestContext, ResolvedInstance,
    TrustedRoot,
};
use hpm_types::credential::{CredentialError, CredentialNeed};
use hpm_types::dispatch::{
    DispatchOutcome, EXECUTE_UNDER_PROTECTION, QUERY_SCRIPT_NOT_ALLOWED, Rendered, UsageSink,
};
use hpm_types::operation::{
    Action, ClassificationSnapshot, ClassificationSource, Operation, OperationSet,
    ScriptOperandGuard, Subject, is_script_value,
};
use hpm_types::protect::{
    DataProtector, InputVerdict, OnUnanalyzable, ProtectError, Protected, ProtectionContext,
    ProtectionRules,
};
use hpm_types::tool::{
    AccessClass, HandlerCompletion, NormalizeInput, OutputClass, ResultBody, RetryEligibility,
    SideEffectClass, ToolDescriptor, ToolError, ToolHandler,
};
use tokio_util::sync::CancellationToken;

const READ_TOOL: &str = "SN-Query-Table";
const REMOTE_TOOL: &str = "SN-Get-Record";
const WRITE_TOOL: &str = "SN-Update-Record";

/// One key per test process: `mint` succeeds once.
static KEY: LazyLock<AdmissionKey> =
    LazyLock::new(|| AdmissionKey::mint().expect("first mint in this process"));

fn key() -> AdmissionKey {
    *KEY
}

struct NoProgress;
impl ProgressSink for NoProgress {
    fn emit(&self, _: f64, _: Option<f64>, _: Option<String>) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
}

struct Echo(Arc<AtomicUsize>);
impl ToolHandler for Echo {
    fn call<'a>(
        &'a self,
        ctx: &'a RequestContext,
        args: serde_json::Value,
    ) -> BoxFut<'a, HandlerCompletion> {
        self.0.fetch_add(1, Ordering::SeqCst);
        // Admission attached no credential (CredentialNeed::None).
        assert!(ctx.credential().is_none());
        Box::pin(async move { HandlerCompletion::not_a_mutation(Ok(ResultBody::Json(args))) })
    }
}

/// Read normalizer: a `javascript:` query operand is a query script operand.
fn normalize_read(
    args: &serde_json::Value,
    _: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    let operands = args
        .get("query")
        .and_then(|q| q.as_str())
        .and_then(|q| q.strip_prefix("javascript:"))
        .map(|s| vec![s.to_owned()])
        .unwrap_or_default();
    Ok(OperationSet(vec![
        Operation::new(Subject::Tool(READ_TOOL), Action::Read).query_script_operands(operands),
    ]))
}

/// Write normalizer: `value` is written to `reference_qual` on `incident`.
fn normalize_write(
    args: &serde_json::Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let script = if is_script_value(value) {
        vec!["reference_qual".to_owned()]
    } else {
        vec![]
    };
    Ok(OperationSet(vec![
        Operation::new(Subject::Tool(WRITE_TOOL), Action::Update)
            .on_table("incident", input.classification)
            .written_fields(vec!["reference_qual".into()])
            .written_script_fields(script),
    ]))
}

fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::Json(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

const QUERY_SCHEMA: &str =
    r#"{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}"#;
const WRITE_SCHEMA: &str =
    r#"{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}"#;

fn read_tool(
    name: &'static str,
    credentials: CredentialNeed,
    calls: &Arc<AtomicUsize>,
) -> ToolDescriptor {
    ToolDescriptor::new(
        name,
        "echo",
        QUERY_SCHEMA,
        normalize_read,
        Arc::new(Echo(calls.clone())),
        render,
    )
    .output(OutputClass::Records)
    .credentials(credentials)
}

fn write_tool(calls: &Arc<AtomicUsize>) -> ToolDescriptor {
    ToolDescriptor::new(
        WRITE_TOOL,
        "write",
        WRITE_SCHEMA,
        normalize_write,
        Arc::new(Echo(calls.clone())),
        render,
    )
    .access(AccessClass::Write)
    .output(OutputClass::Records)
    .side_effect(SideEffectClass::Remote)
    .retry(RetryEligibility::BeforeSendOnly)
}

/// Complete snapshot that knows `incident` as an ordinary table.
struct KnownIncident;
impl ClassificationSource for KnownIncident {
    fn snapshot(&self, _: Option<&ResolvedInstance>) -> Arc<ClassificationSnapshot> {
        let mut s = ClassificationSnapshot::default();
        s.complete = true;
        s.known_tables.insert("incident".into());
        Arc::new(s)
    }
}

/// Records attempts and denials.
#[derive(Default)]
struct Recorder {
    attempts: Mutex<Vec<AttemptRecord>>,
    denied: Mutex<Vec<String>>,
}
impl AuditSink for Recorder {
    fn begin_attempt<'a>(
        &'a self,
        a: AttemptRecord,
    ) -> BoxFut<'a, Result<PendingAttempt, AuditError>> {
        let call_id = a.call_id.clone();
        self.attempts.lock().expect("lock").push(a);
        Box::pin(async move { Ok(PendingAttempt { call_id }) })
    }
    fn complete<'a>(
        &'a self,
        _: &'a PendingAttempt,
        _: TerminalOutcome,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        Box::pin(async { Ok(()) })
    }
    fn record<'a>(&'a self, e: AuditEvent) -> BoxFut<'a, Result<(), AuditError>> {
        if let AuditEvent::Denied { reason, .. } = e {
            self.denied.lock().expect("lock").push(reason);
        }
        Box::pin(async { Ok(()) })
    }
    fn failure_policy(&self) -> SinkFailurePolicy {
        SinkFailurePolicy::BestEffort
    }
    fn audits_reads(&self) -> bool {
        false
    }
}

/// Active protection that allows everything except what the pinned
/// `on_unanalyzable` setting makes the dispatcher deny.
struct Protecting(OnUnanalyzable);
impl DataProtector for Protecting {
    fn is_passthrough(&self) -> bool {
        false
    }
    fn pin<'a>(
        &'a self,
        _: &'a RequestContext,
        subject: Subject,
        ops: Arc<OperationSet>,
        output: OutputClass,
    ) -> BoxFut<'a, Result<Arc<ProtectionContext>, ProtectError>> {
        let rules = Arc::new(ProtectionRules {
            version: 7,
            rules: serde_json::Value::Null,
        });
        let p = ProtectionContext::new(subject, ops, output, rules, self.0);
        Box::pin(async move { Ok(Arc::new(p)) })
    }
    fn guard_input<'a>(
        &'a self,
        _: &'a ProtectionContext,
        _: &'a serde_json::Value,
    ) -> BoxFut<'a, Result<InputVerdict, ProtectError>> {
        Box::pin(async { Ok(InputVerdict::Allow) })
    }
    fn protect_result<'a>(
        &'a self,
        _: &'a ProtectionContext,
        value: ResultBody,
    ) -> BoxFut<'a, Result<Protected<ResultBody>, ProtectError>> {
        Box::pin(async move { Ok(Protected { value, withheld: 0 }) })
    }
    fn protect_error<'a>(
        &'a self,
        _: &'a ProtectionContext,
        err: ToolError,
    ) -> BoxFut<'a, ToolError> {
        Box::pin(async move { err })
    }
    fn protect_progress(&self, _: &ProtectionContext, message: String) -> Option<String> {
        Some(message)
    }
    fn protect_resource<'a>(
        &'a self,
        _: &'a ProtectionContext,
        value: serde_json::Value,
    ) -> BoxFut<'a, Result<Protected<serde_json::Value>, ProtectError>> {
        Box::pin(async move { Ok(Protected { value, withheld: 0 }) })
    }
}

fn dispatcher_with(
    calls: &Arc<AtomicUsize>,
    protector: Arc<dyn DataProtector>,
    audit: Arc<Recorder>,
) -> Dispatcher {
    dispatcher_full(calls, protector, audit, None)
}

fn dispatcher_full(
    calls: &Arc<AtomicUsize>,
    protector: Arc<dyn DataProtector>,
    audit: Arc<Recorder>,
    usage: Option<Arc<dyn UsageSink>>,
) -> Dispatcher {
    let mut registry = ToolRegistry::new().expect("registry");
    registry
        .register_tool(read_tool(READ_TOOL, CredentialNeed::None, calls).needs_instance(true))
        .expect("register");
    registry
        .register_tool(read_tool(REMOTE_TOOL, CredentialNeed::Rest, calls))
        .expect("register");
    registry.register_tool(write_tool(calls)).expect("register");
    let services = Services {
        validator: Arc::new(RequiredPropertiesValidator),
        classification: Arc::new(KnownIncident),
        policy: Arc::new(AllowAll),
        protector,
        credentials: Arc::new(LocalOnlyCredentials),
        audit,
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
        usage,
    };
    Dispatcher::new(registry, services, key())
}

fn dispatcher(calls: &Arc<AtomicUsize>) -> Dispatcher {
    dispatcher_with(calls, Arc::new(PassThrough), Arc::new(Recorder::default()))
}

fn ctx() -> RequestContext {
    ctx_with_miss(None)
}

/// `instance_miss`: what the protocol adapter sets for an unknown per-call instance.
fn ctx_with_miss(instance_miss: Option<String>) -> RequestContext {
    TrustedRoot::single_user_stdio()
        .admit(
            AdmissionInput {
                mcp_request_id: "1".into(),
                protocol_version: ProtocolVersion::new("2025-06-18"),
                client_capabilities: serde_json::Value::Null,
                client_name: "test".into(),
                instance: None,
                instance_miss,
                cancel: CancellationToken::new(),
                deadline: Instant::now() + Duration::from_secs(60),
                progress: Arc::new(NoProgress),
            },
            None,
        )
        .expect("stdio admission")
}

#[test]
fn second_mint_fails() {
    let _ = key();
    assert!(AdmissionKey::mint().is_none());
}

#[tokio::test]
async fn renders_handler_result() {
    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher(&calls);
    let out = d
        .call_tool(
            &ctx(),
            READ_TOOL,
            serde_json::json!({"query": "active=true"}),
        )
        .await
        .expect("call");
    assert_eq!(
        out,
        DispatchOutcome::Rendered(Rendered {
            text: r#"{"query":"active=true"}"#.into(),
            is_error: None
        })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Listing follows 6.2.0 source order, not registration order.
    assert_eq!(
        d.tools().iter().map(|t| t.name).collect::<Vec<_>>(),
        [READ_TOOL, REMOTE_TOOL, WRITE_TOOL]
    );
}

#[tokio::test]
async fn unknown_tool_and_missing_property_are_invalid_params() {
    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher(&calls);
    let unknown = d.call_tool(&ctx(), "Nope", serde_json::json!({})).await;
    assert!(matches!(unknown, Err(ToolError::InvalidParams(m)) if m == "Unknown tool: Nope"));
    let missing = d.call_tool(&ctx(), READ_TOOL, serde_json::json!({})).await;
    assert!(matches!(missing, Err(ToolError::InvalidParams(m)) if m.contains("query")));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn handler_validated_tool_sees_schema_invalid_input_others_do_not() {
    // One of the allow-listed tools (`HANDLER_VALIDATED_TOOLS`); the real
    // handlers' refusals are proven in `handler_validated_tools.rs`.
    const SELF_CHECKING: &str = "SN-Get-ATF-Results";
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new().expect("registry");
    // Any other tool is refused the flag at registration (review W3 M4).
    assert!(matches!(
        registry.register_tool(
            read_tool("SN-List-Incidents", CredentialNeed::None, &calls)
                .handler_validates_input(true)
        ),
        Err(RegistrationError::HandlerValidationNotAllowed(
            "SN-List-Incidents"
        ))
    ));
    registry
        .register_tool(read_tool(READ_TOOL, CredentialNeed::None, &calls))
        .expect("register");
    registry
        .register_tool(
            read_tool(SELF_CHECKING, CredentialNeed::None, &calls)
                .output(OutputClass::Opaque)
                .handler_validates_input(true),
        )
        .expect("register");
    let services = Services {
        validator: Arc::new(RequiredPropertiesValidator),
        classification: Arc::new(KnownIncident),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: Arc::new(LocalOnlyCredentials),
        audit: Arc::new(Recorder::default()),
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
        usage: None,
    };
    let d = Dispatcher::new(registry, services, key());
    let bad = serde_json::json!({"secret": "x"});
    // Unflagged: generic -32602 and the handler never runs.
    let strict = d.call_tool(&ctx(), READ_TOOL, bad.clone()).await;
    assert!(
        matches!(strict, Err(ToolError::InvalidParams(_))),
        "{strict:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // Flagged: the handler receives the unchanged bad input (Echo renders it).
    let out = d
        .call_tool(&ctx(), SELF_CHECKING, bad.clone())
        .await
        .expect("handler runs");
    assert_eq!(
        out,
        DispatchOutcome::Rendered(Rendered {
            text: bad.to_string(),
            is_error: None
        })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn query_script_operand_guard_runs_in_single_user_mode() {
    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher(&calls);
    let allowed = d
        .call_tool(
            &ctx(),
            READ_TOOL,
            serde_json::json!({"query": "javascript:gs.daysAgo(7)"}),
        )
        .await;
    assert!(allowed.is_ok());
    let blocked = d
        .call_tool(
            &ctx(),
            READ_TOOL,
            serde_json::json!({"query": "javascript:gs.setProperty('a','b')"}),
        )
        .await;
    assert!(
        matches!(blocked, Err(ToolError::Denied { reason }) if reason == QUERY_SCRIPT_NOT_ALLOWED)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// H4: a written `javascript:` value is not rejected; it escalates the op to Execute.
#[tokio::test]
async fn written_script_value_escalates_to_execute_not_rejected() {
    let calls = Arc::new(AtomicUsize::new(0));
    let audit = Arc::new(Recorder::default());
    let d = dispatcher_with(&calls, Arc::new(PassThrough), audit.clone());
    for value in ["javascript:new MyQual().get()", "active=true"] {
        let out = d
            .call_tool(&ctx(), WRITE_TOOL, serde_json::json!({ "value": value }))
            .await;
        assert!(out.is_ok(), "{value}: {out:?}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let actions: Vec<_> = audit
        .attempts
        .lock()
        .expect("lock")
        .iter()
        .map(|a| a.ops[0].action)
        .collect();
    assert_eq!(actions, [Action::Execute, Action::Update]);
}

/// H3: under active protection with `on_unanalyzable = Deny`, an effective Execute op
/// is denied right after pinning; ordinary writes pass; `PatternsOnly` is the scoped
/// exception.
#[tokio::test]
async fn execute_under_protection_is_denied_unless_scoped_exception() {
    let script = serde_json::json!({ "value": "javascript:x()" });
    let plain = serde_json::json!({ "value": "active=true" });

    let calls = Arc::new(AtomicUsize::new(0));
    let audit = Arc::new(Recorder::default());
    let d = dispatcher_with(
        &calls,
        Arc::new(Protecting(OnUnanalyzable::default())),
        audit.clone(),
    );
    let denied = d.call_tool(&ctx(), WRITE_TOOL, script.clone()).await;
    assert!(
        matches!(denied, Err(ToolError::Denied { reason }) if reason == EXECUTE_UNDER_PROTECTION)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        *audit.denied.lock().expect("lock"),
        [EXECUTE_UNDER_PROTECTION]
    );
    assert!(audit.attempts.lock().expect("lock").is_empty());
    assert!(d.call_tool(&ctx(), WRITE_TOOL, plain).await.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher_with(
        &calls,
        Arc::new(Protecting(OnUnanalyzable::PatternsOnly)),
        Arc::new(Recorder::default()),
    );
    assert!(d.call_tool(&ctx(), WRITE_TOOL, script).await.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn remote_tool_fails_closed_without_credential_provider() {
    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher(&calls);
    let out = d
        .call_tool(&ctx(), REMOTE_TOOL, serde_json::json!({"query": "x"}))
        .await;
    assert!(matches!(
        out,
        Err(ToolError::Credential(CredentialError::Backend(_)))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[derive(Default)]
struct Usage(Mutex<Vec<&'static str>>);
impl UsageSink for Usage {
    fn record(&self, tool: &'static str) {
        self.0.lock().expect("usage").push(tool);
    }
}

#[tokio::test]
async fn usage_sink_counts_registered_tools_only() {
    let calls = Arc::new(AtomicUsize::new(0));
    let usage = Arc::new(Usage::default());
    let d = dispatcher_full(
        &calls,
        Arc::new(PassThrough),
        Arc::new(Recorder::default()),
        Some(usage.clone()),
    );
    let _ = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "x"}))
        .await;
    let _ = d.call_tool(&ctx(), "Nope", serde_json::json!({})).await;
    // Recorded after lookup, before validation.
    let _ = d.call_tool(&ctx(), READ_TOOL, serde_json::json!({})).await;
    assert_eq!(*usage.0.lock().expect("usage"), vec![READ_TOOL, READ_TOOL]);
}

#[tokio::test]
async fn unknown_per_call_instance_is_a_tool_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher(&calls);
    let out = d
        .call_tool(
            &ctx_with_miss(Some("Instance 'ghost' not found".into())),
            READ_TOOL,
            serde_json::json!({"query": "x", "instance": "ghost"}),
        )
        .await;
    assert!(
        matches!(&out, Err(ToolError::Message(m)) if m == "Instance 'ghost' not found"),
        "{out:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn registration_rejects_duplicates_and_bad_schemas() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new().expect("registry");
    registry
        .register_tool(read_tool(READ_TOOL, CredentialNeed::None, &calls))
        .expect("register");
    assert_eq!(
        registry.register_tool(read_tool(READ_TOOL, CredentialNeed::None, &calls)),
        Err(RegistrationError::DuplicateTool(READ_TOOL))
    );
    let mut bad = read_tool(REMOTE_TOOL, CredentialNeed::None, &calls);
    bad.input_schema_json = "[]";
    assert_eq!(
        registry.register_tool(bad),
        Err(RegistrationError::InvalidSchema(REMOTE_TOOL))
    );
}
