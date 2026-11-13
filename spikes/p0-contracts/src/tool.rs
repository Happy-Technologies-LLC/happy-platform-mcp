//! Tool/resource descriptors, classes, handler completion facts and 6.2.0 renderer.

use std::sync::Arc;

use crate::BoxFut;
use crate::context::{RequestContext, ResolvedInstance};
use crate::credential::{CredentialError, CredentialNeed};
use crate::operation::{Action, ClassificationSnapshot, OperationSet};

/// Declared capability of the tool (D05 classifies all 60). The normalizer may
/// produce a more sensitive action (Create on an execute table → Execute) but the
/// produced set must satisfy the declared floor (`floor_satisfied`, edit 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccessClass {
    Metadata,
    Read,
    Write,
    Execute,
    Config,
    /// No instance data: docs/vector search, script-sync filesystem.
    Local,
}

impl AccessClass {
    /// Explicit mapping, no ordinal ordering. A Write tool whose normalized set
    /// contains no mutation is a normalizer bug ⇒ fail closed.
    pub fn floor_satisfied(self, ops: &OperationSet) -> bool {
        let any = |f: fn(Action) -> bool| ops.0.iter().any(|o| f(o.action));
        match self {
            AccessClass::Metadata | AccessClass::Read | AccessClass::Local => true,
            AccessClass::Write => any(Action::is_mutation),
            AccessClass::Execute => any(|a| a == Action::Execute),
            AccessClass::Config => any(|a| matches!(a, Action::ConfigWrite | Action::AdminWrite)),
        }
    }
}

/// What the output may contain (drives DataProtector work). `Schema` is NOT an
/// exemption under active protection (edit 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputClass {
    None,
    Schema,
    Records,
    /// Free-form script output / logs: untrusted record data.
    Opaque,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SideEffectClass {
    None,
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetryEligibility {
    Idempotent,
    BeforeSendOnly,
    /// Ambiguous → MutationOutcome::Unknown; never replayed.
    Never,
}

/// Execution FACTS. Protection never changes these (edit 2); visibility is separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub enum MutationOutcome {
    NotAMutation,
    /// Admitted attempt ended before any request was sent (cancel, credential error).
    NotSent,
    /// PROVEN no mutation committed.
    Failed,
    Committed,
    /// Some operations committed, some did not (batch without rollback,
    /// `servicenow-client.js:2493-2569`). Per-op facts in `HandlerCompletion::ops`.
    PartiallyCommitted,
    /// Sent but confirmation lost (cancel/deadline/disconnect after send). Never replayed.
    Unknown,
}

/// Per-operation fact, indexed into the call's `OperationSet`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct OpCompletion {
    pub op_index: usize,
    pub outcome: MutationOutcome,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub sys_id: Option<String>,
    pub fields: Vec<(String, serde_json::Value)>,
}

/// Typed body. Renderers turn this into 6.2.0 text; DataProtector walks it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ResultBody {
    Records { table: String, records: Vec<Record>, total: Option<u64> },
    Record { table: String, record: Record },
    /// Scanned under protection; unknown shapes blocked.
    Schema(serde_json::Value),
    ScriptOutput { lines: Vec<String> },
    /// Fixed product text with no instance data.
    Message(String),
    /// Scanned under protection; unknown shapes blocked.
    Json(serde_json::Value),
}

/// Every handler return path carries the facts, independent of the error (edit 2).
#[derive(Debug)]
pub struct HandlerCompletion {
    pub outcome: MutationOutcome,
    pub ops: Vec<OpCompletion>,
    pub result: Result<ResultBody, ToolError>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    /// JSON-RPC -32602 (C6): schema validation / unknown tool. Text never echoes values.
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("denied: {reason}")]
    Denied { reason: String },
    #[error(transparent)]
    Credential(#[from] CredentialError),
    /// ServiceNow error. `message` may echo data → protect_error.
    #[error("servicenow {status}: {message}")]
    Upstream { status: u16, message: String },
    #[error("cancelled")]
    Cancelled,
    #[error("deadline exceeded")]
    Deadline,
    #[error("unavailable: {0}")]
    Unavailable(&'static str),
    #[error("internal: {0}")]
    Internal(String),
}

pub trait ToolHandler: Send + Sync {
    /// `args` are exactly the validated, authorized arguments (no rewriting).
    /// Uses `ctx.credential()` (bound at admission) and `ctx.progress()` (protected).
    /// MUST check `ctx.cancel()` before every outbound request; never sends a
    /// mutation after cancellation.
    fn call<'a>(&'a self, ctx: &'a RequestContext, args: serde_json::Value) -> BoxFut<'a, HandlerCompletion>;
}

pub trait ResourceHandler: Send + Sync {
    fn read<'a>(&'a self, ctx: &'a RequestContext) -> BoxFut<'a, Result<serde_json::Value, ToolError>>;
}

/// Inputs to a pure normalizer: validated args + immutable classification (edit 1).
pub struct NormalizeInput<'a> {
    pub instance: Option<&'a ResolvedInstance>,
    pub classification: &'a ClassificationSnapshot,
}

pub type Normalizer = fn(&serde_json::Value, &NormalizeInput<'_>) -> Result<OperationSet, ToolError>;
/// Pure; reproduces 6.2.0 bytes (golden-tested) for visible results.
pub type Renderer = fn(&ResultBody) -> String;

#[derive(Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema_json: &'static str,
    pub access: AccessClass,
    pub output: OutputClass,
    pub side_effect: SideEffectClass,
    pub retry: RetryEligibility,
    pub needs_instance: bool,
    /// Only what the call needs; Local/config tools need none (edit 4).
    pub credentials: CredentialNeed,
    pub normalize: Normalizer,
    pub handler: Arc<dyn ToolHandler>,
    pub render: Renderer,
}

#[derive(Clone)]
pub struct ResourceDescriptor {
    pub uri: &'static str,
    pub output: OutputClass,
    pub credentials: CredentialNeed,
    pub handler: Arc<dyn ResourceHandler>,
}
