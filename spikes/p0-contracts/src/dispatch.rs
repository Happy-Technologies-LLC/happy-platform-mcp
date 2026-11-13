//! Shared dispatcher (stdio and HTTP alike), RFC v0.2 §10:
//! validate → resolve classification → normalize + floor → authorize → pin
//! protection + input guard → prepare bound credential → liveness → durable
//! attempt → handler → retain facts → protect output → finalize audit → safe render.
//! Tools and resources share the path (edit 9).

use std::sync::Arc;

use crate::audit::{AttemptRecord, AuditEvent, AuditSink, OpSummary, PendingAttempt, SinkFailurePolicy, TerminalOutcome};
use crate::context::RequestContext;
use crate::credential::{CredentialError, CredentialNeed, CredentialProvider};
use crate::operation::{
    Action, ClassificationSource, Operation, OperationSet, ScriptOperandGuard, Subject, TableClass,
    script_operand_allowed,
};
use crate::policy::{Decision, PolicyEngine};
use crate::protect::{DataProtector, InputVerdict, ProtectedProgress, ProtectionContext};
use crate::tool::{
    HandlerCompletion, MutationOutcome, NormalizeInput, OutputClass, ResourceDescriptor, ResultBody, ToolDescriptor,
    ToolError,
};

pub trait SchemaValidator: Send + Sync {
    /// Against the exact public inputSchema (C6). Error text must not echo values.
    fn validate(&self, tool: &ToolDescriptor, args: &serde_json::Value) -> Result<(), String>;
}

pub struct Dispatcher {
    /// Sorted in 6.2.0 tools/list order (C5).
    pub tools: Vec<ToolDescriptor>,
    pub resources: Vec<ResourceDescriptor>,
    pub validator: Arc<dyn SchemaValidator>,
    pub classification: Arc<dyn ClassificationSource>,
    pub policy: Arc<dyn PolicyEngine>,
    pub protector: Arc<dyn DataProtector>,
    pub credentials: Arc<dyn CredentialProvider>,
    pub audit: Arc<dyn AuditSink>,
    pub script_guard: ScriptOperandGuard,
}

/// Text sent to the client (6.2.0: single text content item + isError).
#[derive(Debug)]
pub struct Rendered {
    pub text: String,
    pub is_error: bool,
}

/// hpm-protocol maps `ConnectRequired` to a continuation + `ConnectPrompt`
/// (continuation.rs). Nothing has been admitted when it is returned.
#[derive(Debug)]
#[non_exhaustive]
pub enum DispatchOutcome {
    Rendered(Rendered),
    ConnectRequired { connect_url: String },
}

const WITHHELD_COMMITTED: &str =
    "The operation completed on ServiceNow, but its result was withheld by data protection policy. Do not retry.";
const PARTIAL_TEXT: &str =
    "Some changes were committed on ServiceNow and others were not. Do not re-run the whole call; check the affected records first.";
const UNKNOWN_TEXT: &str =
    "The request reached ServiceNow but its result could not be confirmed. Do not retry; check the affected records first.";
const COMMITTED_FOLLOWUP_FAILED: &str =
    "The operation completed on ServiceNow, but a follow-up step failed. Do not retry.";
const AUDIT_COMPLETE_RETRIES: usize = 3;

fn summaries(ops: &OperationSet) -> Vec<OpSummary> {
    ops.0.iter().map(OpSummary::from).collect()
}

/// Admission products shared by tool and resource paths.
struct Admitted {
    ctx: RequestContext,
    pinned: Option<Arc<ProtectionContext>>,
    policy_version: u64,
}

impl Dispatcher {
    /// rmcp 3.10.0 `ServerHandler::get_tool` defaults to `None`, which bypasses
    /// rmcp's lookup-based validation (`handler/server.rs:607-612`). The hand-written
    /// handler MUST delegate `get_tool` here as well as `call_tool`/`list_tools`.
    pub fn get_tool(&self, name: &str) -> Option<&ToolDescriptor> {
        self.tools.iter().find(|t| t.name == name)
    }

    async fn exceptional(&self, ctx: &RequestContext, subject: &Subject, kind: &'static str) {
        let _ = self
            .audit
            .record(AuditEvent::Exceptional {
                call_id: ctx.call_id().clone(),
                principal: ctx.principal().clone(),
                subject: subject.clone(),
                kind,
            })
            .await;
    }

    /// Steps after normalization, shared by tools and resources: authorize, pin
    /// protection, guard input, prepare credential, bind context.
    async fn admit(
        &self,
        ctx: &RequestContext,
        subject: &Subject,
        ops: Arc<OperationSet>,
        output: OutputClass,
        args: &serde_json::Value,
        need: CredentialNeed,
    ) -> Result<Admitted, Result<DispatchOutcome, ToolError>> {
        // Unknown classification fails closed when policy/protection applies.
        if ops.has_unknown_class() && (self.policy.enforces() || !self.protector.is_passthrough()) {
            self.exceptional(ctx, subject, "unclassified_table").await;
            return Err(Err(ToolError::Denied { reason: "UNCLASSIFIED_TABLE".into() }));
        }
        // `javascript:` operand guard (query + written values).
        if ops.0.iter().flat_map(|o| &o.script_operands).any(|s| !script_operand_allowed(s, self.script_guard)) {
            self.exceptional(ctx, subject, "script_operand_blocked").await;
            return Err(Err(ToolError::Denied { reason: "QUERY_SCRIPT_NOT_ALLOWED".into() }));
        }
        let decision = match self.policy.decide(ctx, &ops).await {
            Ok(d) => d,
            Err(_) => {
                self.exceptional(ctx, subject, "policy_unavailable").await;
                return Err(Err(ToolError::Unavailable("policy")));
            }
        };
        if let Decision::Deny { reason } = decision.decision {
            let _ = self
                .audit
                .record(AuditEvent::Denied {
                    call_id: ctx.call_id().clone(),
                    principal: ctx.principal().clone(),
                    ops: summaries(&ops),
                    reason: reason.clone(),
                    policy_version: decision.policy_version,
                })
                .await;
            return Err(Err(ToolError::Denied { reason }));
        }

        let pinned = if self.protector.is_passthrough() {
            None
        } else {
            let pinned = match self.protector.pin(ctx, subject.clone(), ops.clone(), output).await {
                Ok(p) => p,
                Err(_) => {
                    self.exceptional(ctx, subject, "protector_unavailable").await;
                    return Err(Err(ToolError::Unavailable("data protection")));
                }
            };
            match self.protector.guard_input(&pinned, args).await {
                Ok(InputVerdict::Allow) => {}
                Ok(InputVerdict::Block { reason }) => {
                    self.exceptional(ctx, subject, "input_blocked").await;
                    return Err(Err(ToolError::Denied { reason }));
                }
                Err(_) => {
                    self.exceptional(ctx, subject, "protector_unavailable").await;
                    return Err(Err(ToolError::Unavailable("data protection")));
                }
            }
            Some(pinned)
        };

        // Prepare the bound credential. ConnectRequired ⇒ nothing admitted.
        let binding = match self.credentials.prepare(ctx, need).await {
            Ok(b) => b,
            Err(CredentialError::ConnectRequired { connect_url }) => {
                self.exceptional(ctx, subject, "connect_required").await;
                return Err(Ok(DispatchOutcome::ConnectRequired { connect_url }));
            }
            Err(e) => {
                self.exceptional(ctx, subject, "credential_unavailable").await;
                let e = ToolError::Credential(e);
                let e = match &pinned {
                    Some(p) => self.protector.protect_error(p, e).await,
                    None => e,
                };
                return Err(Err(e));
            }
        };

        let progress = match &pinned {
            Some(p) => Arc::new(ProtectedProgress {
                inner: ctx.progress().clone(),
                protector: self.protector.clone(),
                pinned: p.clone(),
            }) as Arc<dyn crate::context::ProgressSink>,
            None => ctx.progress().clone(),
        };
        Ok(Admitted {
            ctx: ctx.admitted(binding, pinned.clone(), progress),
            pinned,
            policy_version: decision.policy_version,
        })
    }

    /// Bounded retry; on exhaustion the attempt stays pending (alerted, visible,
    /// reconciled later). Never changes the response.
    async fn finalize(&self, pending: &PendingAttempt, terminal: TerminalOutcome) {
        for _ in 0..AUDIT_COMPLETE_RETRIES {
            if self.audit.complete(pending, terminal.clone()).await.is_ok() {
                return;
            }
        }
        // R1: emit alert metric + backpressure on further mutation admission.
    }

    pub async fn call_tool(&self, ctx: &RequestContext, name: &str, args: serde_json::Value)
        -> Result<DispatchOutcome, ToolError> {
        // C6: unknown tool → -32602 before anything else.
        let tool = self.get_tool(name).ok_or_else(|| ToolError::InvalidParams(format!("Unknown tool: {name}")))?;
        let subject = Subject::Tool(tool.name);

        if let Err(msg) = self.validator.validate(tool, &args) {
            self.exceptional(ctx, &subject, "invalid_params").await;
            return Err(ToolError::InvalidParams(msg));
        }
        let classification = self.classification.snapshot(ctx.instance());
        let input = NormalizeInput { instance: ctx.instance(), classification: &classification };
        let ops = match (tool.normalize)(&args, &input) {
            Ok(ops) => Arc::new(ops),
            Err(_) => {
                // Pre-admission errors render fixed safe text (no echoed values).
                self.exceptional(ctx, &subject, "normalize_failed").await;
                return Err(ToolError::InvalidParams("Invalid arguments".into()));
            }
        };
        if !tool.access.floor_satisfied(&ops) {
            self.exceptional(ctx, &subject, "access_floor_violated").await;
            return Err(ToolError::Internal("operation classification".into()));
        }

        let admitted = match self.admit(ctx, &subject, ops.clone(), tool.output, &args, tool.credentials).await {
            Ok(a) => a,
            Err(r) => return r,
        };
        let actx = &admitted.ctx;

        if !actx.is_live() {
            self.exceptional(actx, &subject, "cancelled_before_send").await;
            return Err(ToolError::Cancelled);
        }

        let pending = if ops.mutates() {
            let record = AttemptRecord {
                call_id: actx.call_id().clone(),
                mcp_request_id: actx.mcp_request_id().to_owned(),
                principal: actx.principal().clone(),
                ops: summaries(&ops),
                policy_version: admitted.policy_version,
                protection_version: admitted.pinned.as_ref().map_or(0, |p| p.rules.version),
            };
            match self.audit.begin_attempt(record).await {
                Ok(p) => Some(p),
                Err(_) if self.audit.failure_policy() == SinkFailurePolicy::FailClosedForMutations => {
                    self.exceptional(actx, &subject, "audit_unavailable").await;
                    return Err(ToolError::Unavailable("audit"));
                }
                Err(_) => None,
            }
        } else {
            None
        };

        // Handler runs on exactly the validated, authorized args.
        let HandlerCompletion { outcome, ops: op_facts, result } = tool.handler.call(actx, args).await;

        // Protection changes visibility only; `outcome`/`op_facts` are immutable here.
        let (visible, withheld): (Result<Option<ResultBody>, ToolError>, u32) = match (result, &admitted.pinned) {
            (Ok(body), None) => (Ok(Some(body)), 0),
            (Ok(body), Some(p)) => match self.protector.protect_result(p, body).await {
                Ok(pr) => (Ok(Some(pr.value)), pr.withheld),
                Err(_) => (Ok(None), 1),
            },
            (Err(e), None) => (Err(e), 0),
            (Err(e), Some(p)) => (Err(self.protector.protect_error(p, e).await), 0),
        };

        match &pending {
            Some(p) => self.finalize(p, TerminalOutcome { outcome, ops: op_facts, withheld }).await,
            None if visible.is_err() => self.exceptional(actx, &subject, "handler_error").await,
            None if self.audit.audits_reads() => {
                let _ = self
                    .audit
                    .record(AuditEvent::Read {
                        call_id: actx.call_id().clone(),
                        principal: actx.principal().clone(),
                        ops: summaries(&ops),
                        withheld,
                    })
                    .await;
            }
            None => {}
        }

        render(tool, outcome, visible)
    }

    /// `resources/read` through the same admission path (edit 9).
    pub async fn read_resource(&self, ctx: &RequestContext, uri: &str) -> Result<serde_json::Value, ToolError> {
        let res = self
            .resources
            .iter()
            .find(|r| r.uri == uri)
            .ok_or_else(|| ToolError::InvalidParams("Unknown resource".into()))?;
        let subject = Subject::Resource(res.uri.to_owned());
        let ops = Arc::new(OperationSet(vec![Operation {
            subject: subject.clone(),
            instance_id: ctx.instance().map(|i| i.instance_id.clone()),
            table: None,
            action: Action::Read,
            class: TableClass::NotApplicable,
            arg_location: String::new(),
            returned_fields: Vec::new(),
            written_fields: Vec::new(),
            predicate_fields: Vec::new(),
            script_operands: Vec::new(),
            ordering_fields: Vec::new(),
        }]));
        let admitted = match self
            .admit(ctx, &subject, ops.clone(), res.output, &serde_json::Value::Null, res.credentials)
            .await
        {
            Ok(a) => a,
            Err(Ok(DispatchOutcome::ConnectRequired { connect_url })) => {
                return Err(ToolError::Credential(CredentialError::ConnectRequired { connect_url }));
            }
            Err(Ok(_)) => return Err(ToolError::Internal("unexpected admission outcome".into())),
            Err(Err(e)) => return Err(e),
        };
        let actx = &admitted.ctx;
        let (body, withheld) = match (res.handler.read(actx).await, &admitted.pinned) {
            (Ok(b), None) => (b, 0),
            (Ok(b), Some(p)) => match self.protector.protect_resource(p, b).await {
                Ok(pr) => (pr.value, pr.withheld),
                Err(_) => {
                    self.exceptional(actx, &subject, "output_protect_failed").await;
                    return Err(ToolError::Unavailable("data protection"));
                }
            },
            (Err(e), pinned) => {
                self.exceptional(actx, &subject, "handler_error").await;
                return Err(match pinned {
                    Some(p) => self.protector.protect_error(p, e).await,
                    None => e,
                });
            }
        };
        if self.audit.audits_reads() {
            let _ = self
                .audit
                .record(AuditEvent::Read {
                    call_id: actx.call_id().clone(),
                    principal: actx.principal().clone(),
                    ops: summaries(&ops),
                    withheld,
                })
                .await;
        }
        Ok(body)
    }
}

/// Safe render. Ordinary failures keep 6.2.0 parity (`Err` → isError text);
/// committed/partial/unknown use fixed text, never a tool renderer over synthetic data.
fn render(
    tool: &ToolDescriptor,
    outcome: MutationOutcome,
    visible: Result<Option<ResultBody>, ToolError>,
) -> Result<DispatchOutcome, ToolError> {
    let fixed = |text: &str, is_error: bool| Ok(DispatchOutcome::Rendered(Rendered { text: text.into(), is_error }));
    match (visible, outcome) {
        (_, MutationOutcome::PartiallyCommitted) => fixed(PARTIAL_TEXT, true),
        (_, MutationOutcome::Unknown) => fixed(UNKNOWN_TEXT, true),
        (Ok(Some(body)), _) => Ok(DispatchOutcome::Rendered(Rendered { text: (tool.render)(&body), is_error: false })),
        (Ok(None), MutationOutcome::Committed) => fixed(WITHHELD_COMMITTED, false),
        (Ok(None), _) => Err(ToolError::Unavailable("data protection")),
        (Err(_), MutationOutcome::Committed) => fixed(COMMITTED_FOLLOWUP_FAILED, false),
        (Err(e), _) => Err(e),
    }
}
