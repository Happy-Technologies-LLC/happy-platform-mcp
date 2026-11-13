//! Dispatcher pipeline over the single-user services.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hpm_tools::dispatcher::{Dispatcher, RequiredPropertiesValidator, Services};
use hpm_tools::registration::{RegistrationError, ToolRegistry};
use hpm_tools::single_user::{
    AllowAll, LocalOnlyCredentials, PassThrough, StderrAudit, UnloadedClassification,
};
use hpm_types::BoxFut;
use hpm_types::context::{
    AdmissionInput, AdmissionKey, ProgressSink, ProtocolVersion, RequestContext, TrustedRoot,
};
use hpm_types::credential::{CredentialError, CredentialNeed};
use hpm_types::dispatch::{DispatchOutcome, QUERY_SCRIPT_NOT_ALLOWED, Rendered};
use hpm_types::operation::{
    Action, Operation, OperationSet, ScriptOperandGuard, Subject, TableClass,
};
use hpm_types::tool::{
    AccessClass, HandlerCompletion, MutationOutcome, NormalizeInput, OutputClass, ResultBody,
    RetryEligibility, SideEffectClass, ToolDescriptor, ToolError, ToolHandler,
};
use tokio_util::sync::CancellationToken;

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
        Box::pin(async move {
            HandlerCompletion {
                outcome: MutationOutcome::NotAMutation,
                ops: vec![],
                result: Ok(ResultBody::Json(args)),
            }
        })
    }
}

fn read_op(script_operands: Vec<String>) -> OperationSet {
    OperationSet(vec![Operation {
        subject: Subject::Tool("Echo"),
        instance_id: None,
        table: None,
        action: Action::Read,
        class: TableClass::NotApplicable,
        arg_location: String::new(),
        returned_fields: vec![],
        written_fields: vec![],
        predicate_fields: vec![],
        ordering_fields: vec![],
        script_operands,
    }])
}

fn normalize(args: &serde_json::Value, _: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    let operands = args
        .get("query")
        .and_then(|q| q.as_str())
        .and_then(|q| q.strip_prefix("javascript:"))
        .map(|s| vec![s.to_owned()])
        .unwrap_or_default();
    Ok(read_op(operands))
}

fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::Json(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

fn descriptor(
    name: &'static str,
    credentials: CredentialNeed,
    calls: Arc<AtomicUsize>,
) -> ToolDescriptor {
    ToolDescriptor {
        name,
        description: "echo",
        input_schema_json: r#"{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}"#,
        access: AccessClass::Read,
        output: OutputClass::Opaque,
        side_effect: SideEffectClass::None,
        retry: RetryEligibility::Idempotent,
        needs_instance: false,
        credentials,
        normalize,
        handler: Arc::new(Echo(calls)),
        render,
    }
}

fn dispatcher(calls: &Arc<AtomicUsize>) -> Dispatcher {
    let mut registry = ToolRegistry::new();
    registry
        .register_tool(descriptor("Echo", CredentialNeed::None, calls.clone()))
        .expect("register");
    registry
        .register_tool(descriptor("Remote", CredentialNeed::Rest, calls.clone()))
        .expect("register");
    let services = Services {
        validator: Arc::new(RequiredPropertiesValidator),
        classification: Arc::new(UnloadedClassification),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: Arc::new(LocalOnlyCredentials),
        audit: Arc::new(StderrAudit),
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
    };
    Dispatcher::new(registry, services, AdmissionKey::mint())
}

fn ctx() -> RequestContext {
    TrustedRoot::single_user_stdio()
        .admit(
            AdmissionInput {
                mcp_request_id: "1".into(),
                protocol_version: ProtocolVersion::new("2025-06-18"),
                client_capabilities: serde_json::Value::Null,
                client_name: "test".into(),
                instance: None,
                cancel: CancellationToken::new(),
                deadline: Instant::now() + Duration::from_secs(60),
                progress: Arc::new(NoProgress),
            },
            None,
        )
        .expect("stdio admission")
}

#[tokio::test]
async fn renders_handler_result() {
    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher(&calls);
    let out = d
        .call_tool(&ctx(), "Echo", serde_json::json!({"query": "active=true"}))
        .await
        .expect("register");
    assert_eq!(
        out,
        DispatchOutcome::Rendered(Rendered {
            text: r#"{"query":"active=true"}"#.into(),
            is_error: false
        })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        d.tools().iter().map(|t| t.name).collect::<Vec<_>>(),
        ["Echo", "Remote"]
    );
}

#[tokio::test]
async fn unknown_tool_and_missing_property_are_invalid_params() {
    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher(&calls);
    let unknown = d.call_tool(&ctx(), "Nope", serde_json::json!({})).await;
    assert!(matches!(unknown, Err(ToolError::InvalidParams(m)) if m == "Unknown tool: Nope"));
    let missing = d.call_tool(&ctx(), "Echo", serde_json::json!({})).await;
    assert!(matches!(missing, Err(ToolError::InvalidParams(m)) if m.contains("query")));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn script_operand_guard_runs_in_single_user_mode() {
    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher(&calls);
    let allowed = d
        .call_tool(
            &ctx(),
            "Echo",
            serde_json::json!({"query": "javascript:gs.daysAgo(7)"}),
        )
        .await;
    assert!(allowed.is_ok());
    let blocked = d
        .call_tool(
            &ctx(),
            "Echo",
            serde_json::json!({"query": "javascript:gs.setProperty('a','b')"}),
        )
        .await;
    assert!(
        matches!(blocked, Err(ToolError::Denied { reason }) if reason == QUERY_SCRIPT_NOT_ALLOWED)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn remote_tool_fails_closed_without_credential_provider() {
    let calls = Arc::new(AtomicUsize::new(0));
    let d = dispatcher(&calls);
    let out = d
        .call_tool(&ctx(), "Remote", serde_json::json!({"query": "x"}))
        .await;
    assert!(matches!(
        out,
        Err(ToolError::Credential(CredentialError::Backend(_)))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn registration_rejects_duplicates_and_bad_schemas() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register_tool(descriptor("Echo", CredentialNeed::None, calls.clone()))
        .expect("register");
    assert_eq!(
        registry.register_tool(descriptor("Echo", CredentialNeed::None, calls.clone())),
        Err(RegistrationError::DuplicateTool("Echo"))
    );
    let mut bad = descriptor("Bad", CredentialNeed::None, calls);
    bad.input_schema_json = "[]";
    assert_eq!(
        registry.register_tool(bad),
        Err(RegistrationError::InvalidSchema("Bad"))
    );
}
