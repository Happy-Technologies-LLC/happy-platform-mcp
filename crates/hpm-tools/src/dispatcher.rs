//! Shared dispatcher, stdio and HTTP alike (plan §5.2 Placement, RFC v0.2 §10.1):
//! lookup → validate → classify → normalize + access floor → script-operand guard →
//! policy → protection pin + input guard → bound credential → bind context →
//! liveness → durable attempt → handler → protect output → finalize audit → safe
//! render. Tools and resources share the admission path (edit 9).
//!
//! Admission (step 0) happens before this, in the protocol adapter, through
//! `hpm_types::context::TrustedRoot::admit`.

use std::sync::Arc;

use hpm_types::audit::{
    AttemptRecord, AuditEvent, AuditSink, OpSummary, PendingAttempt, SinkFailurePolicy,
    TerminalOutcome,
};
use hpm_types::context::{AdmissionKey, ProgressSink, RequestContext};
use hpm_types::credential::{CredentialError, CredentialNeed, CredentialProvider};
use hpm_types::dispatch::{
    DispatchOutcome, QUERY_SCRIPT_NOT_ALLOWED, Rendered, SchemaValidator, UNCLASSIFIED_TABLE,
};
use hpm_types::operation::{
    Action, ClassificationSource, Operation, OperationSet, ScriptOperandGuard, Subject, TableClass,
    script_operand_allowed,
};
use hpm_types::policy::{Decision, PolicyEngine};
use hpm_types::protect::{DataProtector, InputVerdict, ProtectedProgress, ProtectionContext};
use hpm_types::tool::{
    HandlerCompletion, MutationOutcome, NormalizeInput, OutputClass, ResourceDescriptor,
    ResultBody, ToolDescriptor, ToolError,
};

use crate::registration::ToolRegistry;

const WITHHELD_COMMITTED: &str = "The operation completed on ServiceNow, but its result was withheld by data protection policy. Do not retry.";
const PARTIAL_TEXT: &str = "Some changes were committed on ServiceNow and others were not. Do not re-run the whole call; check the affected records first.";
const UNKNOWN_TEXT: &str = "The request reached ServiceNow but its result could not be confirmed. Do not retry; check the affected records first.";
const COMMITTED_FOLLOWUP_FAILED: &str =
    "The operation completed on ServiceNow, but a follow-up step failed. Do not retry.";
const AUDIT_COMPLETE_RETRIES: usize = 3;

/// Services the dispatcher consults. Single-user defaults live in
/// [`crate::single_user`]; the composition root chooses.
pub struct Services {
    pub validator: Arc<dyn SchemaValidator>,
    pub classification: Arc<dyn ClassificationSource>,
    pub policy: Arc<dyn PolicyEngine>,
    pub protector: Arc<dyn DataProtector>,
    pub credentials: Arc<dyn CredentialProvider>,
    pub audit: Arc<dyn AuditSink>,
    pub script_guard: ScriptOperandGuard,
}

pub struct Dispatcher {
    tools: Vec<ToolDescriptor>,
    resources: Vec<ResourceDescriptor>,
    services: Services,
    key: AdmissionKey,
}

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
    /// `key` is minted by the composition root (feature `trusted-root`); it is the
    /// dispatcher's only authority to attach admission products.
    pub fn new(registry: ToolRegistry, services: Services, key: AdmissionKey) -> Self {
        let (tools, resources) = registry.into_parts();
        Self {
            tools,
            resources,
            services,
            key,
        }
    }

    /// Registered tools in `tools/list` order (C5).
    pub fn tools(&self) -> &[ToolDescriptor] {
        &self.tools
    }

    pub fn resources(&self) -> &[ResourceDescriptor] {
        &self.resources
    }

    /// rmcp 3.10.0 `ServerHandler::get_tool` defaults to `None`, which bypasses
    /// rmcp's lookup-based validation. The protocol handler MUST delegate `get_tool`
    /// here as well as `call_tool`/`list_tools`.
    pub fn get_tool(&self, name: &str) -> Option<&ToolDescriptor> {
        self.tools.iter().find(|t| t.name == name)
    }

    async fn exceptional(&self, ctx: &RequestContext, subject: &Subject, kind: &'static str) {
        // Best effort: an exceptional event never changes the response.
        let _ = self
            .services
            .audit
            .record(AuditEvent::Exceptional {
                call_id: ctx.call_id().clone(),
                principal: ctx.principal().clone(),
                subject: subject.clone(),
                kind,
            })
            .await;
    }

    /// Steps after normalization, shared by tools and resources: script guard,
    /// authorize, pin protection, guard input, prepare credential, bind context.
    async fn admit(
        &self,
        ctx: &RequestContext,
        subject: &Subject,
        ops: Arc<OperationSet>,
        output: OutputClass,
        args: &serde_json::Value,
        need: CredentialNeed,
    ) -> Result<Admitted, Result<DispatchOutcome, ToolError>> {
        let s = &self.services;
        // Unknown classification fails closed when policy or protection applies.
        if ops.has_unknown_class() && (s.policy.enforces() || !s.protector.is_passthrough()) {
            self.exceptional(ctx, subject, "unclassified_table").await;
            return Err(Err(ToolError::Denied {
                reason: UNCLASSIFIED_TABLE.into(),
            }));
        }
        // `javascript:` operand guard (query and written values), every mode.
        if ops
            .0
            .iter()
            .flat_map(|o| &o.script_operands)
            .any(|op| !script_operand_allowed(op, s.script_guard))
        {
            self.exceptional(ctx, subject, "script_operand_blocked")
                .await;
            return Err(Err(ToolError::Denied {
                reason: QUERY_SCRIPT_NOT_ALLOWED.into(),
            }));
        }
        let decision = match s.policy.decide(ctx, &ops).await {
            Ok(d) => d,
            Err(_) => {
                self.exceptional(ctx, subject, "policy_unavailable").await;
                return Err(Err(ToolError::Unavailable("policy")));
            }
        };
        // Fail closed: any decision other than an explicit Allow denies.
        let deny_reason = match decision.decision {
            Decision::Allow => None,
            Decision::Deny { reason } => Some(reason),
            _ => Some("unrecognized policy decision".to_owned()),
        };
        if let Some(reason) = deny_reason {
            let _ = s
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

        let pinned = if s.protector.is_passthrough() {
            None
        } else {
            let pinned = match s
                .protector
                .pin(ctx, subject.clone(), ops.clone(), output)
                .await
            {
                Ok(p) => p,
                Err(_) => {
                    self.exceptional(ctx, subject, "protector_unavailable")
                        .await;
                    return Err(Err(ToolError::Unavailable("data protection")));
                }
            };
            match s.protector.guard_input(&pinned, args).await {
                Ok(InputVerdict::Allow) => {}
                Ok(InputVerdict::Block { reason }) => {
                    self.exceptional(ctx, subject, "input_blocked").await;
                    return Err(Err(ToolError::Denied { reason }));
                }
                // Fail closed on a verdict this dispatcher does not know.
                Ok(_) => {
                    self.exceptional(ctx, subject, "input_blocked").await;
                    return Err(Err(ToolError::Denied {
                        reason: "unrecognized input verdict".into(),
                    }));
                }
                Err(_) => {
                    self.exceptional(ctx, subject, "protector_unavailable")
                        .await;
                    return Err(Err(ToolError::Unavailable("data protection")));
                }
            }
            Some(pinned)
        };

        // Prepare the bound credential. ConnectRequired ⇒ nothing admitted.
        let binding = match s.credentials.prepare(ctx, need).await {
            Ok(b) => b,
            Err(CredentialError::ConnectRequired { connect_url }) => {
                self.exceptional(ctx, subject, "connect_required").await;
                return Err(Ok(DispatchOutcome::ConnectRequired { connect_url }));
            }
            Err(e) => {
                self.exceptional(ctx, subject, "credential_unavailable")
                    .await;
                let e = ToolError::Credential(e);
                let e = match &pinned {
                    Some(p) => s.protector.protect_error(p, e).await,
                    None => e,
                };
                return Err(Err(e));
            }
        };

        let progress: Arc<dyn ProgressSink> = match &pinned {
            Some(p) => Arc::new(ProtectedProgress::new(
                ctx.progress().clone(),
                s.protector.clone(),
                p.clone(),
            )),
            None => ctx.progress().clone(),
        };
        Ok(Admitted {
            ctx: ctx.admitted(&self.key, binding, pinned.clone(), progress),
            pinned,
            policy_version: decision.policy_version,
        })
    }

    /// Bounded retry; on exhaustion the attempt stays pending (reconciled later).
    /// Never changes the response.
    async fn finalize(&self, pending: &PendingAttempt, terminal: TerminalOutcome) {
        for _ in 0..AUDIT_COMPLETE_RETRIES {
            if self
                .services
                .audit
                .complete(pending, terminal.clone())
                .await
                .is_ok()
            {
                return;
            }
        }
    }

    pub async fn call_tool(
        &self,
        ctx: &RequestContext,
        name: &str,
        args: serde_json::Value,
    ) -> Result<DispatchOutcome, ToolError> {
        let s = &self.services;
        // C6: unknown tool → invalid params before anything else.
        let tool = self
            .get_tool(name)
            .ok_or_else(|| ToolError::InvalidParams(format!("Unknown tool: {name}")))?;
        let subject = Subject::Tool(tool.name);

        if let Err(msg) = s.validator.validate(tool, &args) {
            self.exceptional(ctx, &subject, "invalid_params").await;
            return Err(ToolError::InvalidParams(msg));
        }
        let classification = s.classification.snapshot(ctx.instance());
        let input = NormalizeInput {
            instance: ctx.instance(),
            classification: &classification,
        };
        let ops = match (tool.normalize)(&args, &input) {
            Ok(ops) => Arc::new(ops),
            Err(_) => {
                // Pre-admission errors render fixed safe text (no echoed values).
                self.exceptional(ctx, &subject, "normalize_failed").await;
                return Err(ToolError::InvalidParams("Invalid arguments".into()));
            }
        };
        if !tool.access.floor_satisfied(&ops) {
            self.exceptional(ctx, &subject, "access_floor_violated")
                .await;
            return Err(ToolError::Internal("operation classification".into()));
        }

        let admitted = match self
            .admit(
                ctx,
                &subject,
                ops.clone(),
                tool.output,
                &args,
                tool.credentials,
            )
            .await
        {
            Ok(a) => a,
            Err(r) => return r,
        };
        let actx = &admitted.ctx;

        if !actx.is_live() {
            self.exceptional(actx, &subject, "cancelled_before_send")
                .await;
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
            match s.audit.begin_attempt(record).await {
                Ok(p) => Some(p),
                Err(_) if s.audit.failure_policy() == SinkFailurePolicy::FailClosedForMutations => {
                    self.exceptional(actx, &subject, "audit_unavailable").await;
                    return Err(ToolError::Unavailable("audit"));
                }
                Err(_) => None,
            }
        } else {
            None
        };

        // The handler runs on exactly the validated, authorized arguments.
        let HandlerCompletion {
            outcome,
            ops: op_facts,
            result,
        } = tool.handler.call(actx, args).await;

        // Protection changes visibility only; `outcome`/`op_facts` stay as reported.
        let (visible, withheld): (Result<Option<ResultBody>, ToolError>, u32) =
            match (result, &admitted.pinned) {
                (Ok(body), None) => (Ok(Some(body)), 0),
                (Ok(body), Some(p)) => match s.protector.protect_result(p, body).await {
                    Ok(pr) => (Ok(Some(pr.value)), pr.withheld),
                    Err(_) => (Ok(None), 1),
                },
                (Err(e), None) => (Err(e), 0),
                (Err(e), Some(p)) => (Err(s.protector.protect_error(p, e).await), 0),
            };

        match &pending {
            Some(p) => {
                self.finalize(
                    p,
                    TerminalOutcome {
                        outcome,
                        ops: op_facts,
                        withheld,
                    },
                )
                .await
            }
            None if visible.is_err() => self.exceptional(actx, &subject, "handler_error").await,
            None if s.audit.audits_reads() => {
                let _ = s
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
    pub async fn read_resource(
        &self,
        ctx: &RequestContext,
        uri: &str,
    ) -> Result<serde_json::Value, ToolError> {
        let s = &self.services;
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
            ordering_fields: Vec::new(),
            script_operands: Vec::new(),
        }]));
        let admitted = match self
            .admit(
                ctx,
                &subject,
                ops.clone(),
                res.output,
                &serde_json::Value::Null,
                res.credentials,
            )
            .await
        {
            Ok(a) => a,
            Err(Ok(DispatchOutcome::ConnectRequired { connect_url })) => {
                return Err(ToolError::Credential(CredentialError::ConnectRequired {
                    connect_url,
                }));
            }
            Err(Ok(_)) => return Err(ToolError::Internal("unexpected admission outcome".into())),
            Err(Err(e)) => return Err(e),
        };
        let actx = &admitted.ctx;
        let (body, withheld) = match (res.handler.read(actx).await, &admitted.pinned) {
            (Ok(b), None) => (b, 0),
            (Ok(b), Some(p)) => match s.protector.protect_resource(p, b).await {
                Ok(pr) => (pr.value, pr.withheld),
                Err(_) => {
                    self.exceptional(actx, &subject, "output_protect_failed")
                        .await;
                    return Err(ToolError::Unavailable("data protection"));
                }
            },
            (Err(e), pinned) => {
                self.exceptional(actx, &subject, "handler_error").await;
                return Err(match pinned {
                    Some(p) => s.protector.protect_error(p, e).await,
                    None => e,
                });
            }
        };
        if s.audit.audits_reads() {
            let _ = s
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
/// committed/partial/unknown outcomes use fixed text, never a tool renderer over
/// synthetic data.
fn render(
    tool: &ToolDescriptor,
    outcome: MutationOutcome,
    visible: Result<Option<ResultBody>, ToolError>,
) -> Result<DispatchOutcome, ToolError> {
    let fixed = |text: &str, is_error: bool| {
        Ok(DispatchOutcome::Rendered(Rendered {
            text: text.into(),
            is_error,
        }))
    };
    match (visible, outcome) {
        (_, MutationOutcome::PartiallyCommitted) => fixed(PARTIAL_TEXT, true),
        (_, MutationOutcome::Unknown) => fixed(UNKNOWN_TEXT, true),
        (Ok(Some(body)), _) => Ok(DispatchOutcome::Rendered(Rendered {
            text: (tool.render)(&body),
            is_error: false,
        })),
        (Ok(None), MutationOutcome::Committed) => fixed(WITHHELD_COMMITTED, false),
        (Ok(None), _) => Err(ToolError::Unavailable("data protection")),
        (Err(_), MutationOutcome::Committed) => fixed(COMMITTED_FOLLOWUP_FAILED, false),
        (Err(e), _) => Err(e),
    }
}

/// Validator until full JSON Schema validation (C6) lands: arguments must be a JSON
/// object carrying every `required` property of the tool's `inputSchema`. Error text
/// names properties, never values.
pub struct RequiredPropertiesValidator;

impl SchemaValidator for RequiredPropertiesValidator {
    fn validate(&self, tool: &ToolDescriptor, args: &serde_json::Value) -> Result<(), String> {
        let Some(obj) = args.as_object() else {
            return Err("arguments must be an object".into());
        };
        let schema: serde_json::Value = serde_json::from_str(tool.input_schema_json)
            .map_err(|_| "tool input schema is invalid".to_owned())?;
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str());
        for key in required {
            if !obj.contains_key(key) {
                return Err(format!("missing required property: {key}"));
            }
        }
        Ok(())
    }
}
