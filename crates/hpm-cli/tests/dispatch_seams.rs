//! Public seams PS-1 (filtered lists), PS-4 (audit aux) and PS-6 (admission limiter)
//! at the dispatcher. Contract: `public-seams-7.2.0.md` (SEAM-T-01, -04, -06) and
//! `limits.md` LIM-S-01..03. The `AdmissionKey` mints once per process, so every
//! test shares one key.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use hpm_tools::dispatcher::{Dispatcher, RequiredPropertiesValidator, Services};
use hpm_tools::registration::ToolRegistry;
use hpm_tools::single_user::{AllowAll, LocalOnlyCredentials, PassThrough};
use hpm_types::BoxFut;
use hpm_types::audit::{
    AttemptRecord, AuditAux, AuditError, AuditEvent, AuditSink, PendingAttempt, SinkFailurePolicy,
    TerminalOutcome,
};
use hpm_types::context::{
    AdmissionInput, AdmissionKey, ProgressSink, ProtocolVersion, RequestContext, ResolvedInstance,
    TrustedRoot,
};
use hpm_types::credential::CredentialNeed;
use hpm_types::dispatch::{DispatchOutcome, EXECUTE_UNDER_PROTECTION, Rendered};
use hpm_types::limits::{
    Admission, AdmissionKind, AdmissionLimiter, AdmissionPermit, LimitExceeded,
};
use hpm_types::operation::{
    Action, ClassificationSnapshot, ClassificationSource, Operation, OperationSet,
    ScriptOperandGuard, Subject, is_script_value,
};
use hpm_types::policy::{Decision, PolicyDecision, PolicyEngine, PolicyError};
use hpm_types::protect::{
    DataProtector, DpKind, DpSummary, InputVerdict, OnUnanalyzable, ProtectError, Protected,
    ProtectionContext, ProtectionRules,
};
use hpm_types::tool::{
    AccessClass, HandlerCompletion, NormalizeInput, OutputClass, ResourceDescriptor,
    ResourceHandler, ResultBody, RetryEligibility, SideEffectClass, ToolDescriptor, ToolError,
    ToolHandler,
};
use tokio_util::sync::CancellationToken;

const READ_TOOL: &str = "SN-Query-Table";
const REMOTE_TOOL: &str = "SN-Get-Record";
const WRITE_TOOL: &str = "SN-Update-Record";
const RES_A: &str = "servicenow://instance";
const RES_B: &str = "servicenow://other";

static KEY: LazyLock<AdmissionKey> =
    LazyLock::new(|| AdmissionKey::mint().expect("first mint in this process"));

struct NoProgress;
impl ProgressSink for NoProgress {
    fn emit(&self, _: f64, _: Option<f64>, _: Option<String>) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
}

/// Counts calls; `query == "boom"` fails the call.
struct Echo(Arc<AtomicUsize>);
impl ToolHandler for Echo {
    fn call<'a>(
        &'a self,
        _: &'a RequestContext,
        args: serde_json::Value,
    ) -> BoxFut<'a, HandlerCompletion> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let boom = args.get("query").and_then(|q| q.as_str()) == Some("boom");
        Box::pin(async move {
            HandlerCompletion::not_a_mutation(if boom {
                Err(ToolError::Message("boom".into()))
            } else {
                Ok(ResultBody::Json(args))
            })
        })
    }
}

struct ResEcho(Arc<AtomicUsize>);
impl ResourceHandler for ResEcho {
    fn read<'a>(
        &'a self,
        _: &'a RequestContext,
    ) -> BoxFut<'a, Result<serde_json::Value, ToolError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(serde_json::json!({"ok": true})) })
    }
}

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
    // `query == "deny-me"` classifies as the other tool's subject, so a policy can
    // deny it while the same tool's ordinary calls stay allowed.
    let subject = if args.get("query").and_then(|q| q.as_str()) == Some("deny-me") {
        Subject::Tool(REMOTE_TOOL)
    } else {
        Subject::Tool(READ_TOOL)
    };
    Ok(OperationSet(vec![
        Operation::new(subject, Action::Read).query_script_operands(operands),
    ]))
}

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
    calls: &Arc<AtomicUsize>,
    renderer: fn(&ResultBody) -> String,
) -> ToolDescriptor {
    ToolDescriptor::new(
        name,
        "echo",
        QUERY_SCHEMA,
        normalize_read,
        Arc::new(Echo(calls.clone())),
        renderer,
    )
    .output(OutputClass::Records)
    .credentials(CredentialNeed::None)
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

struct KnownIncident;
impl ClassificationSource for KnownIncident {
    fn snapshot(&self, _: Option<&ResolvedInstance>) -> Arc<ClassificationSnapshot> {
        let mut s = ClassificationSnapshot::default();
        s.complete = true;
        s.known_tables.insert("incident".into());
        Arc::new(s)
    }
}

// ---------------------------------------------------------------- policy

/// Enforcing engine: `visible` lists what is in `visible`, `decide` denies what is
/// in `deny` (by tool name or resource URI).
struct Rules {
    enforce: bool,
    visible: Vec<&'static str>,
    deny: Vec<&'static str>,
}

fn name_of(s: &Subject) -> String {
    match s {
        Subject::Tool(n) => (*n).to_owned(),
        Subject::Resource(u) => u.clone(),
        _ => String::new(),
    }
}

impl PolicyEngine for Rules {
    fn decide<'a>(
        &'a self,
        _: &'a RequestContext,
        ops: &'a OperationSet,
    ) -> BoxFut<'a, Result<PolicyDecision, PolicyError>> {
        let denied = ops
            .0
            .iter()
            .any(|o| self.deny.contains(&name_of(&o.subject).as_str()));
        Box::pin(async move {
            Ok(PolicyDecision {
                decision: if denied {
                    Decision::Deny {
                        reason: "POLICY_DENIED".into(),
                    }
                } else {
                    Decision::Allow
                },
                policy_version: 3,
            })
        })
    }
    fn visible<'a>(&'a self, _: &'a RequestContext, subject: &'a Subject) -> BoxFut<'a, bool> {
        let v = self.visible.contains(&name_of(subject).as_str());
        Box::pin(async move { v })
    }
    fn enforces(&self) -> bool {
        self.enforce
    }
}

// ----------------------------------------------------------------- audit

#[derive(Clone, Debug)]
struct Seen {
    /// `begin`, `complete`, `denied:<reason>`, `exceptional:<kind>`, `read`.
    what: String,
    tool: String,
    has_ops: bool,
    has_args: bool,
    dp: Option<DpSummary>,
    elapsed_set: bool,
    request_id: String,
}

/// Overrides only the `_with` methods; the legacy ones must never be reached by the
/// dispatcher.
#[derive(Default)]
struct Spy {
    seen: Mutex<Vec<Seen>>,
    fail_begin: bool,
}

impl Spy {
    fn push(&self, what: String, aux: &AuditAux<'_>) {
        self.seen.lock().expect("lock").push(Seen {
            what,
            tool: aux.tool.to_owned(),
            has_ops: aux.ops.is_some(),
            has_args: aux.args.is_some(),
            dp: aux.dp.clone(),
            elapsed_set: aux.elapsed > Duration::ZERO,
            request_id: aux.ctx.mcp_request_id().to_owned(),
        });
    }
    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().expect("lock"))
    }
}

impl AuditSink for Spy {
    fn begin_attempt<'a>(
        &'a self,
        _: AttemptRecord,
    ) -> BoxFut<'a, Result<PendingAttempt, AuditError>> {
        panic!("legacy begin_attempt reached")
    }
    fn complete<'a>(
        &'a self,
        _: &'a PendingAttempt,
        _: TerminalOutcome,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        panic!("legacy complete reached")
    }
    fn record<'a>(&'a self, _: AuditEvent) -> BoxFut<'a, Result<(), AuditError>> {
        panic!("legacy record reached")
    }
    fn failure_policy(&self) -> SinkFailurePolicy {
        if self.fail_begin {
            SinkFailurePolicy::FailClosedForMutations
        } else {
            SinkFailurePolicy::BestEffort
        }
    }
    fn audits_reads(&self) -> bool {
        true
    }
    fn begin_attempt_with<'a>(
        &'a self,
        a: AttemptRecord,
        aux: AuditAux<'a>,
    ) -> BoxFut<'a, Result<PendingAttempt, AuditError>> {
        self.push("begin".into(), &aux);
        let fail = self.fail_begin;
        Box::pin(async move {
            if fail {
                Err(AuditError::Backpressure)
            } else {
                Ok(PendingAttempt { call_id: a.call_id })
            }
        })
    }
    fn complete_with<'a>(
        &'a self,
        _: &'a PendingAttempt,
        _: TerminalOutcome,
        aux: AuditAux<'a>,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        self.push("complete".into(), &aux);
        Box::pin(async { Ok(()) })
    }
    fn record_with<'a>(
        &'a self,
        e: AuditEvent,
        aux: AuditAux<'a>,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        let what = match &e {
            AuditEvent::Denied { reason, .. } => format!("denied:{reason}"),
            AuditEvent::Exceptional { kind, .. } => format!("exceptional:{kind}"),
            AuditEvent::Read { .. } => "read".into(),
            _ => "other".into(),
        };
        self.push(what, &aux);
        Box::pin(async { Ok(()) })
    }
}

/// A sink that implements only the 7.1.0 methods (SEAM-T-04, AUD-S-01).
#[derive(Default)]
struct LegacyOnly {
    log: Mutex<Vec<&'static str>>,
}
impl AuditSink for LegacyOnly {
    fn begin_attempt<'a>(
        &'a self,
        a: AttemptRecord,
    ) -> BoxFut<'a, Result<PendingAttempt, AuditError>> {
        self.log.lock().expect("lock").push("begin");
        Box::pin(async move { Ok(PendingAttempt { call_id: a.call_id }) })
    }
    fn complete<'a>(
        &'a self,
        _: &'a PendingAttempt,
        _: TerminalOutcome,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        self.log.lock().expect("lock").push("complete");
        Box::pin(async { Ok(()) })
    }
    fn record<'a>(&'a self, e: AuditEvent) -> BoxFut<'a, Result<(), AuditError>> {
        self.log.lock().expect("lock").push(match e {
            AuditEvent::Read { .. } => "read",
            AuditEvent::Denied { .. } => "denied",
            AuditEvent::Exceptional { .. } => "exceptional",
            _ => "other",
        });
        Box::pin(async { Ok(()) })
    }
    fn failure_policy(&self) -> SinkFailurePolicy {
        SinkFailurePolicy::BestEffort
    }
    fn audits_reads(&self) -> bool {
        true
    }
}

// ------------------------------------------------------------ protection

/// Active protection that fills the call's tally: profile and rule at pin, a guard
/// block for `query == "blockme"`, field masks on results, pattern masks on resources.
struct Tallying(OnUnanalyzable);
impl DataProtector for Tallying {
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
        p.tally().set_profile("pii");
        p.tally().note_rule("binding-1");
        Box::pin(async move { Ok(Arc::new(p)) })
    }
    fn guard_input<'a>(
        &'a self,
        p: &'a ProtectionContext,
        args: &'a serde_json::Value,
    ) -> BoxFut<'a, Result<InputVerdict, ProtectError>> {
        let block = args.get("query").and_then(|q| q.as_str()) == Some("blockme");
        if block {
            p.tally().add(DpKind::Blocked, 1);
            p.tally().set_guard("DATA_PROTECTION_FORBIDDEN_OPERAND");
        }
        Box::pin(async move {
            Ok(if block {
                InputVerdict::Block {
                    reason: "DATA_PROTECTION_FORBIDDEN_OPERAND".into(),
                }
            } else {
                InputVerdict::Allow
            })
        })
    }
    fn protect_result<'a>(
        &'a self,
        p: &'a ProtectionContext,
        value: ResultBody,
    ) -> BoxFut<'a, Result<Protected<ResultBody>, ProtectError>> {
        p.tally().add(DpKind::MaskedField, 2);
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
        p: &'a ProtectionContext,
        value: serde_json::Value,
    ) -> BoxFut<'a, Result<Protected<serde_json::Value>, ProtectError>> {
        p.tally().add(DpKind::MaskedPattern, 3);
        Box::pin(async move { Ok(Protected { value, withheld: 0 }) })
    }
}

// --------------------------------------------------------------- limiter

/// Live permits are counted in `live`; `refuse` makes every `admit` fail.
struct Gate {
    live: Arc<AtomicUsize>,
    refuse: Option<LimitExceeded>,
    seen: Mutex<Vec<(String, AdmissionKind, bool, bool)>>,
}

struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Gate {
    fn new(live: Arc<AtomicUsize>, refuse: Option<LimitExceeded>) -> Arc<Self> {
        Arc::new(Self {
            live,
            refuse,
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl AdmissionLimiter for Gate {
    fn admit(&self, req: Admission<'_>) -> Result<AdmissionPermit, LimitExceeded> {
        self.seen.lock().expect("lock").push((
            req.tool.to_owned(),
            req.kind,
            req.ops.mutates(),
            req.ops.has_execute(),
        ));
        if let Some(e) = &self.refuse {
            return Err(e.clone());
        }
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(AdmissionPermit::new(Slot(self.live.clone())))
    }
}

// ---------------------------------------------------------------- wiring

struct Parts {
    calls: Arc<AtomicUsize>,
    policy: Arc<dyn PolicyEngine>,
    protector: Arc<dyn DataProtector>,
    audit: Arc<dyn AuditSink>,
    renderer: fn(&ResultBody) -> String,
    limiter: Option<Arc<dyn AdmissionLimiter>>,
    resource_handler: Option<Arc<dyn ResourceHandler>>,
}

impl Parts {
    fn new(audit: Arc<dyn AuditSink>) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            policy: Arc::new(AllowAll),
            protector: Arc::new(PassThrough),
            audit,
            renderer: render,
            limiter: None,
            resource_handler: None,
        }
    }

    fn build(self) -> (Dispatcher, Arc<AtomicUsize>) {
        let calls = self.calls.clone();
        let mut registry = ToolRegistry::new().expect("registry");
        registry
            .register_tool(read_tool(READ_TOOL, &calls, self.renderer).needs_instance(true))
            .expect("register");
        registry
            .register_tool(read_tool(REMOTE_TOOL, &calls, self.renderer))
            .expect("register");
        registry
            .register_tool(write_tool(&calls))
            .expect("register");
        let rh = self
            .resource_handler
            .unwrap_or_else(|| Arc::new(ResEcho(calls.clone())));
        for (uri, name) in [(RES_A, "a"), (RES_B, "b")] {
            registry
                .register_resource(ResourceDescriptor::new(
                    uri,
                    name,
                    "res",
                    "application/json",
                    OutputClass::Records,
                    rh.clone(),
                ))
                .expect("resource");
        }
        let services = Services {
            validator: Arc::new(RequiredPropertiesValidator),
            classification: Arc::new(KnownIncident),
            policy: self.policy,
            protector: self.protector,
            credentials: Arc::new(LocalOnlyCredentials),
            audit: self.audit,
            script_guard: ScriptOperandGuard {
                shared_account: false,
            },
            usage: None,
        };
        let mut d = Dispatcher::new(registry, services, *KEY);
        if let Some(l) = self.limiter {
            d = d.with_admission_limiter(l);
        }
        (d, calls)
    }
}

fn ctx() -> RequestContext {
    ctx_with_cancel(CancellationToken::new())
}

fn ctx_with_cancel(cancel: CancellationToken) -> RequestContext {
    TrustedRoot::single_user_stdio()
        .admit(
            AdmissionInput {
                mcp_request_id: "req-9".into(),
                protocol_version: ProtocolVersion::new("2025-06-18"),
                client_capabilities: serde_json::Value::Null,
                client_name: "test".into(),
                instance: None,
                instance_miss: None,
                cancel,
                deadline: Instant::now() + Duration::from_secs(60),
                progress: Arc::new(NoProgress),
            },
            None,
        )
        .expect("stdio admission")
}

fn tool_names(v: &[&ToolDescriptor]) -> Vec<&'static str> {
    v.iter().map(|t| t.name).collect()
}

// ------------------------------------------------------------ SEAM-T-01

#[tokio::test]
async fn seam_t01_non_enforcing_lists_are_the_registry_in_registry_order() {
    let (d, _) = Parts::new(Arc::new(LegacyOnly::default())).build();
    let all: Vec<_> = d.tools().iter().map(|t| t.name).collect();
    assert_eq!(tool_names(&d.visible_tools(&ctx()).await), all);
    let uris: Vec<_> = d.resources().iter().map(|r| r.uri).collect();
    let got: Vec<_> = d
        .visible_resources(&ctx())
        .await
        .iter()
        .map(|r| r.uri)
        .collect();
    assert_eq!(got, uris);

    // `enforces() == false` returns every tool even if `visible` would say no.
    let mut p = Parts::new(Arc::new(LegacyOnly::default()));
    p.policy = Arc::new(Rules {
        enforce: false,
        visible: vec![],
        deny: vec![],
    });
    let (d, _) = p.build();
    assert_eq!(d.visible_tools(&ctx()).await.len(), d.tools().len());
    assert_eq!(d.visible_resources(&ctx()).await.len(), d.resources().len());
}

#[tokio::test]
async fn seam_t01_enforcing_engine_filters_in_registry_order() {
    let mut p = Parts::new(Arc::new(LegacyOnly::default()));
    p.policy = Arc::new(Rules {
        enforce: true,
        visible: vec![WRITE_TOOL, READ_TOOL, RES_B],
        deny: vec![],
    });
    let (d, _) = p.build();
    let registry: Vec<_> = d.tools().iter().map(|t| t.name).collect();
    let want: Vec<_> = registry
        .iter()
        .copied()
        .filter(|n| *n == READ_TOOL || *n == WRITE_TOOL)
        .collect();
    assert_eq!(want.len(), 2);
    assert_eq!(tool_names(&d.visible_tools(&ctx()).await), want);
    let res: Vec<_> = d
        .visible_resources(&ctx())
        .await
        .iter()
        .map(|r| r.uri)
        .collect();
    assert_eq!(res, [RES_B]);
}

#[tokio::test]
async fn seam_t01_caller_with_no_grant_sees_nothing_and_hidden_tool_answers_policy_denied() {
    let mut p = Parts::new(Arc::new(LegacyOnly::default()));
    p.policy = Arc::new(Rules {
        enforce: true,
        visible: vec![],
        deny: vec![READ_TOOL, RES_A],
    });
    let (d, calls) = p.build();
    assert!(d.visible_tools(&ctx()).await.is_empty());
    assert!(d.visible_resources(&ctx()).await.is_empty());
    // `get_tool` stays unfiltered; calling a hidden tool is a policy denial, never
    // "Unknown tool".
    assert!(d.get_tool(READ_TOOL).is_some());
    let out = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "x"}))
        .await;
    assert!(matches!(out, Err(ToolError::Denied { reason }) if reason == "POLICY_DENIED"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// ------------------------------------------------------------ SEAM-T-04

#[tokio::test]
async fn seam_t04_default_sink_delegates_to_the_legacy_methods() {
    let legacy = Arc::new(LegacyOnly::default());
    let (d, _) = Parts::new(legacy.clone()).build();
    let ok = serde_json::json!({"value": "v"});
    d.call_tool(&ctx(), WRITE_TOOL, ok).await.expect("write");
    d.call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "q"}))
        .await
        .expect("read");
    let _ = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({}))
        .await
        .expect_err("invalid");
    d.read_resource(&ctx(), &d.resources()[0])
        .await
        .expect("resource");
    // begin + complete for the mutation, a Read per read, one exceptional.
    let log = legacy.log.lock().expect("lock").clone();
    assert_eq!(
        log,
        ["begin", "complete", "read", "exceptional", "read"],
        "{log:?}"
    );
}

#[tokio::test]
async fn seam_t04_every_site_calls_the_with_methods_with_aux() {
    let spy = Arc::new(Spy::default());
    let mut p = Parts::new(spy.clone());
    p.protector = Arc::new(Tallying(OnUnanalyzable::Deny));
    p.policy = Arc::new(Rules {
        enforce: true,
        visible: vec![],
        deny: vec![REMOTE_TOOL],
    });
    let (d, _) = p.build();
    let c = ctx();

    // Mutation: begin + complete. Pin-time notes are in begin's dp, results in complete's.
    d.call_tool(&c, WRITE_TOOL, serde_json::json!({"value": "v"}))
        .await
        .expect("write");
    let s = spy.take();
    assert_eq!(s.len(), 2, "{s:?}");
    assert_eq!(s[0].what, "begin");
    assert_eq!(s[1].what, "complete");
    for x in &s {
        assert_eq!(x.tool, WRITE_TOOL);
        assert!(x.has_ops && x.has_args && x.elapsed_set, "{x:?}");
        assert_eq!(x.request_id, "req-9");
    }
    let begin = s[0].dp.as_ref().expect("dp at begin");
    assert_eq!(begin.profile.as_deref(), Some("pii"));
    assert_eq!(begin.rule_ids, ["binding-1"]);
    assert_eq!(begin.masked_field, 0);
    let done = s[1].dp.as_ref().expect("dp at complete");
    assert_eq!(done.masked_field, 2);
    assert_eq!(done.masked(), 2);

    // Read record.
    d.call_tool(&c, READ_TOOL, serde_json::json!({"query": "q"}))
        .await
        .expect("read");
    let s = spy.take();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].what, "read");
    assert!(s[0].has_ops && s[0].has_args);
    assert_eq!(s[0].dp.as_ref().expect("dp").masked_field, 2);

    // Policy denial.
    let _ = d
        .call_tool(&c, READ_TOOL, serde_json::json!({"query": "deny-me"}))
        .await
        .expect_err("denied");
    let s = spy.take();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].what, "denied:POLICY_DENIED");
    assert!(s[0].has_ops && s[0].has_args);
    assert!(s[0].dp.is_none(), "denied before any pin");

    // Q2 denial after pin carries the pin-time dp.
    let _ = d
        .call_tool(&c, WRITE_TOOL, serde_json::json!({"value": "javascript:x"}))
        .await
        .expect_err("execute");
    let s = spy.take();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].what, format!("denied:{EXECUTE_UNDER_PROTECTION}"));
    assert_eq!(
        s[0].dp.as_ref().and_then(|d| d.profile.clone()).as_deref(),
        Some("pii")
    );

    // Input guard block carries the guard code.
    let _ = d
        .call_tool(&c, READ_TOOL, serde_json::json!({"query": "blockme"}))
        .await
        .expect_err("blocked");
    let s = spy.take();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].what, "exceptional:input_blocked");
    let dp = s[0].dp.as_ref().expect("dp");
    assert_eq!(dp.blocked, 1);
    assert_eq!(
        dp.guard.as_deref(),
        Some("DATA_PROTECTION_FORBIDDEN_OPERAND")
    );
    assert!(s[0].has_ops && s[0].has_args);

    // Schema failure: before normalization, no ops, no validated args, no pin.
    let _ = d
        .call_tool(&c, READ_TOOL, serde_json::json!({}))
        .await
        .expect_err("invalid");
    let s = spy.take();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].what, "exceptional:invalid_params");
    assert_eq!(s[0].tool, READ_TOOL);
    assert!(!s[0].has_ops && !s[0].has_args && s[0].dp.is_none());

    // Handler failure on a read.
    let _ = d
        .call_tool(&c, READ_TOOL, serde_json::json!({"query": "boom"}))
        .await
        .expect_err("handler error");
    let s = spy.take();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].what, "exceptional:handler_error");
    assert!(s[0].has_ops && s[0].has_args);

    // Cancelled before send.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let _ = d
        .call_tool(
            &ctx_with_cancel(cancel),
            READ_TOOL,
            serde_json::json!({"query": "q"}),
        )
        .await
        .expect_err("cancelled");
    let s = spy.take();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].what, "exceptional:cancelled_before_send");
    assert!(s[0].dp.is_some());

    // Resource read: empty tool name, ops, no args, dp from the pin.
    d.read_resource(&c, &d.resources()[0])
        .await
        .expect("resource");
    let s = spy.take();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].what, "read");
    assert_eq!(s[0].tool, "");
    assert!(s[0].has_ops && !s[0].has_args);
    assert_eq!(s[0].dp.as_ref().expect("dp").masked_pattern, 3);
}

#[tokio::test]
async fn seam_t04_audit_unavailable_goes_through_record_with() {
    let spy = Arc::new(Spy {
        fail_begin: true,
        ..Spy::default()
    });
    let (d, calls) = Parts::new(spy.clone()).build();
    let out = d
        .call_tool(&ctx(), WRITE_TOOL, serde_json::json!({"value": "v"}))
        .await;
    assert!(matches!(out, Err(ToolError::Unavailable("audit"))));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let s = spy.take();
    assert_eq!(
        s.iter().map(|x| x.what.as_str()).collect::<Vec<_>>(),
        ["begin", "exceptional:audit_unavailable"]
    );
    assert!(s[1].has_ops && s[1].has_args);
}

// ------------------------------------------------------------ SEAM-T-06

#[tokio::test]
async fn seam_t06_default_dispatcher_has_no_limiter_and_unchanged_output() {
    let (d, _) = Parts::new(Arc::new(LegacyOnly::default())).build();
    let out = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "q"}))
        .await
        .expect("call");
    assert_eq!(
        out,
        DispatchOutcome::Rendered(Rendered {
            text: r#"{"query":"q"}"#.into(),
            is_error: None
        })
    );
}

#[tokio::test]
async fn seam_t06_refusing_limiter_blocks_handler_attempt_and_records_exceptional() {
    let spy = Arc::new(Spy::default());
    let gate = Gate::new(
        Arc::default(),
        Some(LimitExceeded::new(
            "limit_rate",
            "rate",
            Some(Duration::from_millis(1500)),
        )),
    );
    let mut p = Parts::new(spy.clone());
    p.limiter = Some(gate.clone());
    let (d, calls) = p.build();

    // A mutating call: no handler, no attempt row.
    let out = d
        .call_tool(&ctx(), WRITE_TOOL, serde_json::json!({"value": "v"}))
        .await;
    match out {
        Err(e) => {
            let ToolError::RateLimited {
                scope,
                retry_after_secs,
            } = &e
            else {
                panic!("{e:?}")
            };
            assert_eq!(*scope, "rate");
            assert_eq!(*retry_after_secs, Some(2), "rounded up");
            assert_eq!(e.to_string(), "RATE_LIMITED: rate");
            assert_eq!(
                hpm_types::limits::rate_limited_text(scope, *retry_after_secs),
                "RATE_LIMITED: rate (retry after 2 s)"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let s = spy.take();
    assert_eq!(s.len(), 1, "no begin_attempt: {s:?}");
    assert_eq!(s[0].what, "exceptional:limit_rate");
    assert_eq!(s[0].tool, WRITE_TOOL);
    assert!(s[0].has_ops && s[0].has_args);

    // A read, and a resource read.
    let out = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "q"}))
        .await;
    assert!(matches!(out, Err(ToolError::RateLimited { .. })));
    let out = d.read_resource(&ctx(), &d.resources()[0]).await;
    assert!(matches!(
        out,
        Err(ToolError::RateLimited { scope: "rate", .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let kinds: Vec<_> = spy.take().into_iter().map(|x| (x.what, x.tool)).collect();
    assert_eq!(
        kinds,
        [
            ("exceptional:limit_rate".to_owned(), READ_TOOL.to_owned()),
            ("exceptional:limit_rate".to_owned(), String::new())
        ]
    );
}

#[tokio::test]
async fn seam_t06_unknown_retry_has_no_secs_and_limiter_sees_the_admission() {
    let gate = Gate::new(
        Arc::default(),
        Some(LimitExceeded::new("limit_inflight", "inflight", None)),
    );
    let mut p = Parts::new(Arc::new(LegacyOnly::default()));
    p.limiter = Some(gate.clone());
    let (d, _) = p.build();
    let out = d
        .call_tool(
            &ctx(),
            WRITE_TOOL,
            serde_json::json!({"value": "javascript:x"}),
        )
        .await;
    assert!(matches!(
        out,
        Err(ToolError::RateLimited {
            scope: "inflight",
            retry_after_secs: None
        })
    ));
    let _ = d.read_resource(&ctx(), &d.resources()[1]).await;
    let seen = gate.seen.lock().expect("lock").clone();
    assert_eq!(
        seen,
        [
            (WRITE_TOOL.to_owned(), AdmissionKind::ToolCall, true, true),
            (String::new(), AdmissionKind::ResourceRead, false, false),
        ]
    );
}

#[tokio::test]
async fn seam_t06_limiter_runs_only_after_policy_allow() {
    let gate = Gate::new(Arc::default(), None);
    let mut p = Parts::new(Arc::new(LegacyOnly::default()));
    p.limiter = Some(gate.clone());
    p.policy = Arc::new(Rules {
        enforce: true,
        visible: vec![],
        deny: vec![REMOTE_TOOL],
    });
    let (d, _) = p.build();
    let out = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "deny-me"}))
        .await;
    assert!(matches!(out, Err(ToolError::Denied { .. })));
    assert!(gate.seen.lock().expect("lock").is_empty());
    d.call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "q"}))
        .await
        .expect("allowed");
    assert_eq!(gate.seen.lock().expect("lock").len(), 1);
}

// ----------------------------------------------------------- LIM-S-01

static PROBE_LIVE: LazyLock<Arc<AtomicUsize>> = LazyLock::new(Arc::default);
static HANDLER_SAW: AtomicUsize = AtomicUsize::new(usize::MAX);
static RENDER_SAW: AtomicUsize = AtomicUsize::new(usize::MAX);

struct Probe;
impl ToolHandler for Probe {
    fn call<'a>(
        &'a self,
        _: &'a RequestContext,
        args: serde_json::Value,
    ) -> BoxFut<'a, HandlerCompletion> {
        HANDLER_SAW.store(PROBE_LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
        Box::pin(async move { HandlerCompletion::not_a_mutation(Ok(ResultBody::Json(args))) })
    }
}

fn render_probe(body: &ResultBody) -> String {
    RENDER_SAW.store(PROBE_LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
    render(body)
}

#[tokio::test]
async fn lim_s01_permit_is_held_from_before_the_handler_through_render() {
    let gate = Gate::new(PROBE_LIVE.clone(), None);
    let spy = Arc::new(Spy::default());
    let mut registry = ToolRegistry::new().expect("registry");
    registry
        .register_tool(
            ToolDescriptor::new(
                READ_TOOL,
                "probe",
                QUERY_SCHEMA,
                normalize_read,
                Arc::new(Probe),
                render_probe,
            )
            .output(OutputClass::Records)
            .credentials(CredentialNeed::None),
        )
        .expect("register");
    let services = Services {
        validator: Arc::new(RequiredPropertiesValidator),
        classification: Arc::new(KnownIncident),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: Arc::new(LocalOnlyCredentials),
        audit: spy.clone(),
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
        usage: None,
    };
    let d = Dispatcher::new(registry, services, *KEY).with_admission_limiter(gate);
    assert_eq!(PROBE_LIVE.load(Ordering::SeqCst), 0);
    let out = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "q"}))
        .await
        .expect("call");
    assert!(matches!(out, DispatchOutcome::Rendered(_)));
    assert_eq!(HANDLER_SAW.load(Ordering::SeqCst), 1, "held at the handler");
    assert_eq!(RENDER_SAW.load(Ordering::SeqCst), 1, "held at render");
    assert_eq!(PROBE_LIVE.load(Ordering::SeqCst), 0, "released on return");
}

#[tokio::test]
async fn lim_s01_permit_is_released_on_every_path() {
    let live = Arc::new(AtomicUsize::new(0));
    let gate = Gate::new(live.clone(), None);
    let mut p = Parts::new(Arc::new(LegacyOnly::default()));
    p.limiter = Some(gate);
    p.protector = Arc::new(Tallying(OnUnanalyzable::Deny));
    let (d, _) = p.build();
    // Success, handler error, input block, Q2 denial after the permit, resource.
    let _ = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "q"}))
        .await;
    let _ = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "boom"}))
        .await;
    let _ = d
        .call_tool(&ctx(), READ_TOOL, serde_json::json!({"query": "blockme"}))
        .await;
    let _ = d
        .call_tool(
            &ctx(),
            WRITE_TOOL,
            serde_json::json!({"value": "javascript:x"}),
        )
        .await;
    let _ = d.read_resource(&ctx(), &d.resources()[0]).await;
    assert_eq!(live.load(Ordering::SeqCst), 0);
}
