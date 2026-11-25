//! AuditSink: durable pending attempt → terminal outcome (review edit 5), plus
//! privacy-safe denial/exceptional/read events (errata §5.8).

use crate::BoxFut;
use crate::context::{CallId, Principal, RequestContext};
use crate::operation::{Action, Operation, OperationSet, Subject};
use crate::protect::DpSummary;
use crate::tool::{MutationOutcome, OpCompletion};

/// Privacy-safe projection: protected values removed BEFORE any hashing.
#[derive(Clone, Debug)]
pub struct OpSummary {
    pub subject: Subject,
    pub instance_id: Option<String>,
    pub table: Option<String>,
    pub action: Action,
    pub field_names: Vec<String>,
    pub projected_args_digest: Option<[u8; 32]>,
}

impl From<&Operation> for OpSummary {
    fn from(o: &Operation) -> Self {
        let mut field_names = o.returned_fields.clone();
        field_names.extend(o.written_fields.iter().cloned());
        field_names.extend(o.predicate_fields.iter().cloned());
        field_names.extend(o.ordering_fields.iter().cloned());
        Self {
            subject: o.subject.clone(),
            instance_id: o.instance_id.clone(),
            table: o.table.clone(),
            action: o.action,
            field_names,
            projected_args_digest: None,
        }
    }
}

/// Written durably (commit) BEFORE any mutation is sent.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AttemptRecord {
    pub call_id: CallId,
    /// Correlation only.
    pub mcp_request_id: String,
    pub principal: Principal,
    pub ops: Vec<OpSummary>,
    pub policy_version: u64,
    pub protection_version: u64,
}

impl AttemptRecord {
    pub fn new(
        call_id: CallId,
        mcp_request_id: String,
        principal: Principal,
        ops: Vec<OpSummary>,
        policy_version: u64,
        protection_version: u64,
    ) -> Self {
        Self {
            call_id,
            mcp_request_id,
            principal,
            ops,
            policy_version,
            protection_version,
        }
    }
}

/// Handle to a durable pending row. Every one gets a terminal outcome or stays
/// visibly pending; after a crash, unresolved = `Unknown` unless reconciled.
/// Never replayed.
#[derive(Debug)]
pub struct PendingAttempt {
    pub call_id: CallId,
}

#[derive(Clone, Debug)]
pub struct TerminalOutcome {
    pub outcome: MutationOutcome,
    pub ops: Vec<OpCompletion>,
    pub withheld: u32,
}

/// Non-attempt events: one exceptional path for validation, normalization,
/// denial, input block and pre-send cancellation.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AuditEvent {
    Denied {
        call_id: CallId,
        principal: Principal,
        ops: Vec<OpSummary>,
        reason: String,
        policy_version: u64,
    },
    Exceptional {
        call_id: CallId,
        principal: Principal,
        subject: Subject,
        kind: &'static str,
    },
    /// Successful reads, when configured.
    Read {
        call_id: CallId,
        principal: Principal,
        ops: Vec<OpSummary>,
        withheld: u32,
    },
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuditError {
    #[error("audit sink unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("audit backpressure")]
    Backpressure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SinkFailurePolicy {
    BestEffort,
    /// MU: no durable pending attempt ⇒ mutation not admitted.
    FailClosedForMutations,
}

/// Everything a sink needs to write a spec §5.8 record that the legacy
/// [`AttemptRecord`] / [`TerminalOutcome`] / [`AuditEvent`] do not carry (PS-4).
/// Passed to [`AuditSink::begin_attempt_with`], [`AuditSink::complete_with`] and
/// [`AuditSink::record_with`]. Build with [`AuditAux::new`] and the `with_*` setters.
#[derive(Clone)]
#[non_exhaustive]
pub struct AuditAux<'a> {
    /// Principal, client, scopes, transport, instance, MCP request id.
    pub ctx: &'a RequestContext,
    /// Registered tool name; `""` for resource reads.
    pub tool: &'a str,
    /// `None` before normalization (exceptional paths).
    pub ops: Option<&'a OperationSet>,
    /// Validated arguments. The sink projects them; it never stores them.
    pub args: Option<&'a serde_json::Value>,
    /// The call's [`crate::protect::DpTally`] snapshot; `None` when no protection
    /// context was pinned.
    pub dp: Option<DpSummary>,
    /// Dispatcher entry to this record.
    pub elapsed: std::time::Duration,
}

impl<'a> AuditAux<'a> {
    pub fn new(ctx: &'a RequestContext, tool: &'a str) -> Self {
        Self {
            ctx,
            tool,
            ops: None,
            args: None,
            dp: None,
            elapsed: std::time::Duration::ZERO,
        }
    }

    pub fn with_ops(mut self, ops: &'a OperationSet) -> Self {
        self.ops = Some(ops);
        self
    }

    pub fn with_args(mut self, args: &'a serde_json::Value) -> Self {
        self.args = Some(args);
        self
    }

    pub fn with_dp(mut self, dp: Option<DpSummary>) -> Self {
        self.dp = dp;
        self
    }

    pub fn with_elapsed(mut self, d: std::time::Duration) -> Self {
        self.elapsed = d;
        self
    }
}

pub trait AuditSink: Send + Sync {
    fn begin_attempt<'a>(
        &'a self,
        a: AttemptRecord,
    ) -> BoxFut<'a, Result<PendingAttempt, AuditError>>;
    /// ONE local transaction: update the attempt row + insert the exporter-outbox
    /// entry. Failure never turns a committed mutation into a failed tool result:
    /// caller retries (bounded, backpressure + alert); the row stays pending.
    fn complete<'a>(
        &'a self,
        p: &'a PendingAttempt,
        t: TerminalOutcome,
    ) -> BoxFut<'a, Result<(), AuditError>>;
    fn record<'a>(&'a self, e: AuditEvent) -> BoxFut<'a, Result<(), AuditError>>;
    fn failure_policy(&self) -> SinkFailurePolicy;
    /// Whether successful reads emit `AuditEvent::Read`.
    fn audits_reads(&self) -> bool;

    /// [`Self::begin_attempt`] plus the call's [`AuditAux`]. The dispatcher calls
    /// this one; the default drops `aux` and delegates, so a sink that implements
    /// only the 7.1.0 methods keeps receiving the legacy calls.
    fn begin_attempt_with<'a>(
        &'a self,
        a: AttemptRecord,
        aux: AuditAux<'a>,
    ) -> BoxFut<'a, Result<PendingAttempt, AuditError>> {
        let _ = aux;
        self.begin_attempt(a)
    }

    /// [`Self::complete`] plus the call's [`AuditAux`]. Default delegates.
    fn complete_with<'a>(
        &'a self,
        p: &'a PendingAttempt,
        t: TerminalOutcome,
        aux: AuditAux<'a>,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        let _ = aux;
        self.complete(p, t)
    }

    /// [`Self::record`] plus the call's [`AuditAux`]. Default delegates.
    fn record_with<'a>(
        &'a self,
        e: AuditEvent,
        aux: AuditAux<'a>,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        let _ = aux;
        self.record(e)
    }
}
