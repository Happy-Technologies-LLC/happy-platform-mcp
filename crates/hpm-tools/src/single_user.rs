//! Single-user service implementations (plan §5.2): no policy enforcement, no data
//! protection, audit to stderr. Enterprise implementations replace these in the
//! composition root; the dispatcher is the same.

use std::sync::Arc;

use hpm_types::BoxFut;
use hpm_types::audit::{
    AttemptRecord, AuditError, AuditEvent, AuditSink, PendingAttempt, SinkFailurePolicy,
    TerminalOutcome,
};
use hpm_types::context::{RequestContext, ResolvedInstance};
use hpm_types::credential::{
    Credential, CredentialBinding, CredentialError, CredentialNeed, CredentialProvider, UiSession,
    UiSessionScope,
};
use hpm_types::operation::{ClassificationSnapshot, ClassificationSource, OperationSet, Subject};
use hpm_types::policy::{Decision, PolicyDecision, PolicyEngine, PolicyError};
use hpm_types::protect::{DataProtector, InputVerdict, ProtectError, Protected, ProtectionContext};
use hpm_types::tool::{OutputClass, ResultBody, ToolError};

/// Single-user policy: the operator owns the account, so every operation is allowed
/// and nothing is filtered from listings. `enforces() == false`.
pub struct AllowAll;

impl PolicyEngine for AllowAll {
    fn decide<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _ops: &'a OperationSet,
    ) -> BoxFut<'a, Result<PolicyDecision, PolicyError>> {
        Box::pin(async {
            Ok(PolicyDecision {
                decision: Decision::Allow,
                policy_version: 0,
            })
        })
    }
    fn visible<'a>(&'a self, _ctx: &'a RequestContext, _subject: &'a Subject) -> BoxFut<'a, bool> {
        Box::pin(async { true })
    }
    fn enforces(&self) -> bool {
        false
    }
}

/// No data protection. The dispatcher skips `pin` when `is_passthrough()`, so the
/// other methods are identity functions kept for completeness of the contract.
pub struct PassThrough;

impl DataProtector for PassThrough {
    fn is_passthrough(&self) -> bool {
        true
    }
    fn pin<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _subject: Subject,
        _ops: Arc<OperationSet>,
        _output: OutputClass,
    ) -> BoxFut<'a, Result<Arc<ProtectionContext>, ProtectError>> {
        Box::pin(async {
            Err(ProtectError::Unavailable(
                "pass-through protector has no protection context".into(),
            ))
        })
    }
    fn guard_input<'a>(
        &'a self,
        _p: &'a ProtectionContext,
        _args: &'a serde_json::Value,
    ) -> BoxFut<'a, Result<InputVerdict, ProtectError>> {
        Box::pin(async { Ok(InputVerdict::Allow) })
    }
    fn protect_result<'a>(
        &'a self,
        _p: &'a ProtectionContext,
        body: ResultBody,
    ) -> BoxFut<'a, Result<Protected<ResultBody>, ProtectError>> {
        Box::pin(async move {
            Ok(Protected {
                value: body,
                withheld: 0,
            })
        })
    }
    fn protect_error<'a>(
        &'a self,
        _p: &'a ProtectionContext,
        err: ToolError,
    ) -> BoxFut<'a, ToolError> {
        Box::pin(async move { err })
    }
    fn protect_progress(&self, _p: &ProtectionContext, message: String) -> Option<String> {
        Some(message)
    }
    fn protect_resource<'a>(
        &'a self,
        _p: &'a ProtectionContext,
        body: serde_json::Value,
    ) -> BoxFut<'a, Result<Protected<serde_json::Value>, ProtectError>> {
        Box::pin(async move {
            Ok(Protected {
                value: body,
                withheld: 0,
            })
        })
    }
}

/// Audit to stderr, one line per event, best effort. Field names and operation
/// summaries only: never argument values, results or secrets. stdout is reserved
/// for JSON-RPC (C10).
pub struct StderrAudit;

impl AuditSink for StderrAudit {
    fn begin_attempt<'a>(
        &'a self,
        a: AttemptRecord,
    ) -> BoxFut<'a, Result<PendingAttempt, AuditError>> {
        Box::pin(async move {
            eprintln!(
                "audit attempt call_id={} mcp_request_id={} ops={:?}",
                a.call_id.as_str(),
                a.mcp_request_id,
                a.ops
            );
            Ok(PendingAttempt { call_id: a.call_id })
        })
    }
    fn complete<'a>(
        &'a self,
        p: &'a PendingAttempt,
        t: TerminalOutcome,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        Box::pin(async move {
            eprintln!(
                "audit complete call_id={} outcome={:?} ops={:?} withheld={}",
                p.call_id.as_str(),
                t.outcome,
                t.ops,
                t.withheld
            );
            Ok(())
        })
    }
    fn record<'a>(&'a self, e: AuditEvent) -> BoxFut<'a, Result<(), AuditError>> {
        Box::pin(async move {
            eprintln!("audit event {e:?}");
            Ok(())
        })
    }
    fn failure_policy(&self) -> SinkFailurePolicy {
        SinkFailurePolicy::BestEffort
    }
    fn audits_reads(&self) -> bool {
        false
    }
}

/// Single-user table classification: the bundled D-05 static baselines only. The
/// snapshot is incomplete (no per-instance `sys_dictionary` load), so tables outside
/// the baselines classify as `Unknown`; that fails closed only where policy or
/// protection applies, which single-user never does (RFC §3.1).
pub struct UnloadedClassification;

impl ClassificationSource for UnloadedClassification {
    fn snapshot(&self, _instance: Option<&ResolvedInstance>) -> Arc<ClassificationSnapshot> {
        Arc::new(ClassificationSnapshot::default())
    }
}

/// Credential provider for compositions with no ServiceNow access (docs-only
/// servers, tests). Any need other than `None` fails closed. The default root wires
/// `hpm_core::credential::provider::SingleUserCredentialProvider`.
pub struct LocalOnlyCredentials;

impl LocalOnlyCredentials {
    fn unavailable() -> CredentialError {
        CredentialError::Backend("no ServiceNow credential provider is configured".into())
    }
}

impl CredentialProvider for LocalOnlyCredentials {
    fn prepare<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        need: CredentialNeed,
    ) -> BoxFut<'a, Result<Option<CredentialBinding>, CredentialError>> {
        Box::pin(async move {
            match need {
                CredentialNeed::None => Ok(None),
                _ => Err(Self::unavailable()),
            }
        })
    }
    fn authorize<'a>(
        &'a self,
        _binding: &'a CredentialBinding,
    ) -> BoxFut<'a, Result<Credential, CredentialError>> {
        Box::pin(async { Err(Self::unavailable()) })
    }
    fn invalidate<'a>(
        &'a self,
        _binding: &'a CredentialBinding,
    ) -> BoxFut<'a, Result<(), CredentialError>> {
        Box::pin(async { Err(Self::unavailable()) })
    }
    fn ui_session<'a>(
        &'a self,
        _binding: &'a CredentialBinding,
        _scope: UiSessionScope,
    ) -> BoxFut<'a, Result<UiSession, CredentialError>> {
        Box::pin(async { Err(Self::unavailable()) })
    }
    fn end_ui_session<'a>(
        &'a self,
        _session: UiSession,
    ) -> BoxFut<'a, Result<(), CredentialError>> {
        Box::pin(async { Err(Self::unavailable()) })
    }
}
