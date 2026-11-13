//! AuditSink: durable pending attempt → terminal outcome (review edit 5), plus
//! privacy-safe denial/exceptional/read events (errata §5.8).

use crate::BoxFut;
use crate::context::{CallId, Principal};
use crate::operation::{Action, Operation, Subject};
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
pub struct AttemptRecord {
    pub call_id: CallId,
    /// Correlation only.
    pub mcp_request_id: String,
    pub principal: Principal,
    pub ops: Vec<OpSummary>,
    pub policy_version: u64,
    pub protection_version: u64,
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
}
