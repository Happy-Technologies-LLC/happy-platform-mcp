//! Tool/resource descriptors, classes, handler completion facts (RFC §9).

use std::sync::Arc;

use crate::BoxFut;
use crate::context::{RequestContext, ResolvedInstance};
use crate::credential::{CredentialError, CredentialNeed};
use crate::operation::{Action, ClassificationSnapshot, OperationSet};

/// Declared capability floor of a tool. The normalizer may produce a more sensitive
/// action (Create on an execute table becomes Execute), but the produced set must
/// satisfy the declared floor (`floor_satisfied`, edit 1).
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
    /// Explicit mapping, no ordinal ordering. A Write tool whose normalized set has
    /// no mutation, or an Execute tool with no Execute operation, is a normalizer
    /// defect: the dispatcher fails closed with `Internal`.
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

/// What the output may contain (drives DataProtector work). `Schema` is not an
/// exemption under active protection (edit 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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
    /// Ambiguous completion is `MutationOutcome::Unknown`; never replayed.
    Never,
}

/// Execution facts. Protection never changes these (edit 2); visibility is separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub enum MutationOutcome {
    NotAMutation,
    /// Admitted attempt ended before any request was sent (cancel, credential error).
    NotSent,
    /// Proven that no mutation committed.
    Failed,
    Committed,
    /// Some operations committed, some did not (batch without rollback). Per-op
    /// facts are in `HandlerCompletion::ops`.
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
#[non_exhaustive]
pub struct Record {
    pub sys_id: Option<String>,
    pub fields: Vec<(String, serde_json::Value)>,
}

impl Record {
    pub fn new(sys_id: Option<String>, fields: Vec<(String, serde_json::Value)>) -> Self {
        Self { sys_id, fields }
    }
}

/// Typed body. Renderers turn this into 6.2.0 text; the DataProtector walks it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ResultBody {
    Records {
        table: String,
        records: Vec<Record>,
        total: Option<u64>,
    },
    Record {
        table: String,
        record: Record,
    },
    /// Scanned under protection; unknown shapes blocked.
    Schema(serde_json::Value),
    ScriptOutput {
        lines: Vec<String>,
    },
    /// Fixed product text with no instance data.
    Message(String),
    /// Scanned under protection; unknown shapes blocked.
    Json(serde_json::Value),
    /// Verbatim 6.2.0 success text (no prefix). `explicit_is_error_false: true`
    /// emits `"isError": false` (ATF `jsonResponse(payload, false)`); `false` omits
    /// the field. Errors are never `Ok`: use `ToolError::Text`. Carries instance
    /// data, so it is scanned under protection like `Json`, never fixed text.
    ToolResult {
        text: String,
        explicit_is_error_false: bool,
    },
}

/// Every handler return path carries the facts, independent of the error (edit 2).
#[derive(Debug)]
#[non_exhaustive]
pub struct HandlerCompletion {
    pub outcome: MutationOutcome,
    pub ops: Vec<OpCompletion>,
    pub result: Result<ResultBody, ToolError>,
}

impl HandlerCompletion {
    pub fn new(
        outcome: MutationOutcome,
        ops: Vec<OpCompletion>,
        result: Result<ResultBody, ToolError>,
    ) -> Self {
        Self {
            outcome,
            ops,
            result,
        }
    }
    /// A call that mutated nothing (`MutationOutcome::NotAMutation`, no op facts).
    pub fn not_a_mutation(result: Result<ResultBody, ToolError>) -> Self {
        Self::new(MutationOutcome::NotAMutation, Vec::new(), result)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    /// JSON-RPC -32602 (C6): schema validation / unknown tool. Text never echoes values.
    #[error("invalid params: {0}")]
    InvalidParams(String),
    /// `reason` is a stable code (e.g. `QUERY_SCRIPT_NOT_ALLOWED`); no values.
    #[error("denied: {reason}")]
    Denied { reason: String },
    #[error(transparent)]
    Credential(#[from] CredentialError),
    /// ServiceNow error. `message` is the 6.2.0 `enrichServiceNowError` text (it
    /// already starts with the status) and may echo data, so it goes through
    /// `protect_error`. Tools render `Error: {message}`.
    #[error("{message}")]
    Upstream { status: u16, message: String },
    /// 6.2.0 `error.message`, shown verbatim: tools render `Error: {message}` with
    /// `isError`, resources answer JSON-RPC -32603 with `{message}`. May echo data,
    /// so it goes through `protect_error` like `Upstream`.
    #[error("{0}")]
    Message(String),
    /// 6.2.0 `isError` text rendered verbatim with no `Error: ` prefix (e.g. `❌ …`,
    /// `Batch update completed with errors:\n…`). A failure for audit and retry,
    /// protected like `Message`, and may accompany any `MutationOutcome`
    /// (PartiallyCommitted batch stop-on-error / rollback, Committed script that
    /// reported errors): the handler's text always wins over generic texts.
    #[error("{0}")]
    Text(String),
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
    /// `args` are exactly the validated, authorized arguments (no rewriting). Uses
    /// `ctx.credential()` (bound at admission) and `ctx.progress()` (protected). Must
    /// check `ctx.cancel()` before every outbound request and never send a mutation
    /// after cancellation.
    fn call<'a>(
        &'a self,
        ctx: &'a RequestContext,
        args: serde_json::Value,
    ) -> BoxFut<'a, HandlerCompletion>;
}

pub trait ResourceHandler: Send + Sync {
    fn read<'a>(
        &'a self,
        ctx: &'a RequestContext,
    ) -> BoxFut<'a, Result<serde_json::Value, ToolError>>;
}

/// Inputs to a pure normalizer: validated args plus immutable classification (edit 1).
/// The classification snapshot is the only per-instance metadata a normalizer gets:
/// normalizers classify, they never look up schema. Build with [`NormalizeInput::new`].
#[non_exhaustive]
pub struct NormalizeInput<'a> {
    pub instance: Option<&'a ResolvedInstance>,
    pub classification: &'a ClassificationSnapshot,
}

impl<'a> NormalizeInput<'a> {
    pub fn new(
        instance: Option<&'a ResolvedInstance>,
        classification: &'a ClassificationSnapshot,
    ) -> Self {
        Self {
            instance,
            classification,
        }
    }
}

pub type Normalizer =
    fn(&serde_json::Value, &NormalizeInput<'_>) -> Result<OperationSet, ToolError>;

/// Pure; reproduces 6.2.0 bytes (golden-tested) for visible results.
pub type Renderer = fn(&ResultBody) -> String;

/// MCP tool annotations (D4). Advisory hints for clients, attached centrally at
/// registration from the D-05 row's `annotation_hints`; never read by policy,
/// protection or the dispatcher.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolAnnotations {
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

/// `#[non_exhaustive]`: build with [`ToolDescriptor::new`] and the setters. Defaults
/// (`Read`, `OutputClass::None`, `SideEffectClass::None`, `Idempotent`, no instance,
/// `CredentialNeed::None`) must be overridden to match the tool's D-05 row; the
/// registry rejects any mismatch.
#[derive(Clone)]
#[non_exhaustive]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    /// Exact 6.2.0 schema bytes.
    pub input_schema_json: &'static str,
    pub access: AccessClass,
    pub output: OutputClass,
    pub side_effect: SideEffectClass,
    pub retry: RetryEligibility,
    pub needs_instance: bool,
    /// Only what the call needs; local/config tools need none (edit 4).
    pub credentials: CredentialNeed,
    pub normalize: Normalizer,
    pub handler: Arc<dyn ToolHandler>,
    pub render: Renderer,
    /// Optional display title (MCP `title`). Advisory only.
    pub title: Option<&'static str>,
    /// Set by the tool registry from the D-05 row; a value set by a lane is
    /// overwritten. Advisory only.
    pub annotations: Option<ToolAnnotations>,
}

impl ToolDescriptor {
    pub fn new(
        name: &'static str,
        description: &'static str,
        input_schema_json: &'static str,
        normalize: Normalizer,
        handler: Arc<dyn ToolHandler>,
        render: Renderer,
    ) -> Self {
        Self {
            name,
            description,
            input_schema_json,
            access: AccessClass::Read,
            output: OutputClass::None,
            side_effect: SideEffectClass::None,
            retry: RetryEligibility::Idempotent,
            needs_instance: false,
            credentials: CredentialNeed::None,
            normalize,
            handler,
            render,
            title: None,
            annotations: None,
        }
    }
    pub fn access(mut self, access: AccessClass) -> Self {
        self.access = access;
        self
    }
    pub fn output(mut self, output: OutputClass) -> Self {
        self.output = output;
        self
    }
    pub fn side_effect(mut self, side_effect: SideEffectClass) -> Self {
        self.side_effect = side_effect;
        self
    }
    pub fn retry(mut self, retry: RetryEligibility) -> Self {
        self.retry = retry;
        self
    }
    pub fn needs_instance(mut self, needs_instance: bool) -> Self {
        self.needs_instance = needs_instance;
        self
    }
    pub fn credentials(mut self, credentials: CredentialNeed) -> Self {
        self.credentials = credentials;
        self
    }
    pub fn title(mut self, title: &'static str) -> Self {
        self.title = Some(title);
        self
    }
}

#[derive(Clone)]
#[non_exhaustive]
pub struct ResourceDescriptor {
    pub uri: &'static str,
    /// 6.2.0 `resources/list` fields.
    pub name: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
    pub output: OutputClass,
    pub credentials: CredentialNeed,
    pub handler: Arc<dyn ResourceHandler>,
}

impl ResourceDescriptor {
    pub fn new(
        uri: &'static str,
        name: &'static str,
        description: &'static str,
        mime_type: &'static str,
        output: OutputClass,
        credentials: CredentialNeed,
        handler: Arc<dyn ResourceHandler>,
    ) -> Self {
        Self {
            uri,
            name,
            description,
            mime_type,
            output,
            credentials,
            handler,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{Operation, Subject};

    fn ops(actions: &[Action]) -> OperationSet {
        OperationSet(
            actions
                .iter()
                .map(|&action| Operation::new(Subject::Tool("T"), action))
                .collect(),
        )
    }

    #[test]
    fn descriptor_floor_uses_explicit_mapping() {
        use AccessClass as C;
        use Action as A;
        let cases: &[(C, &[A], bool)] = &[
            (C::Read, &[], true),
            (C::Metadata, &[A::Metadata], true),
            (C::Local, &[], true),
            // A Write tool must produce some mutation; escalation to Execute counts.
            (C::Write, &[A::Read], false),
            (C::Write, &[], false),
            (C::Write, &[A::Read, A::Update], true),
            (C::Write, &[A::Execute], true),
            (C::Write, &[A::AdminWrite], true),
            // Execute needs an Execute op; other mutations do not satisfy it.
            (C::Execute, &[A::Create, A::Update, A::Delete], false),
            (C::Execute, &[A::AdminWrite], false),
            (C::Execute, &[A::Read, A::Execute], true),
            // Config needs a config or admin write; Execute is not a config change.
            (C::Config, &[A::Read], false),
            (C::Config, &[A::Execute], false),
            (C::Config, &[A::ConfigWrite], true),
            (C::Config, &[A::AdminWrite], true),
        ];
        for &(class, actions, expected) in cases {
            assert_eq!(
                class.floor_satisfied(&ops(actions)),
                expected,
                "{class:?} {actions:?}"
            );
        }
    }

    #[test]
    fn only_metadata_and_read_are_not_mutations() {
        use Action as A;
        for a in [A::Metadata, A::Read] {
            assert!(!a.is_mutation(), "{a:?}");
        }
        for a in [
            A::Create,
            A::Update,
            A::Delete,
            A::Execute,
            A::AdminWrite,
            A::ConfigWrite,
        ] {
            assert!(a.is_mutation(), "{a:?}");
        }
    }
}
