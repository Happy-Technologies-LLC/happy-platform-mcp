//! DataProtector: input guards + output hooks over typed values (F2), all pinned
//! to one immutable `ProtectionContext` per call (review edit 3).

use std::sync::Arc;

use crate::BoxFut;
use crate::context::ProgressSink;
use crate::context::RequestContext;
use crate::operation::{OperationSet, Subject};
use crate::tool::{OutputClass, ResultBody, ToolError};

/// Selected versioned rules. Opaque to the dispatcher.
#[derive(Debug)]
pub struct ProtectionRules {
    pub version: u64,
    pub rules: serde_json::Value,
}

/// Built once at admission; never reloaded between channels of one call.
#[derive(Debug)]
pub struct ProtectionContext {
    pub subject: Subject,
    pub ops: Arc<OperationSet>,
    pub output: OutputClass,
    pub rules: Arc<ProtectionRules>,
}

/// D-06 profile setting for tools whose accessed tables cannot be resolved statically
/// (scripts, workflows). Owner decision 2026-11-13 (RFC §13 Q2): `Deny` is the default
/// whenever a profile applies; `PatternsOnly` is an explicit, scoped, audited exception.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OnUnanalyzable {
    #[default]
    Deny,
    PatternsOnly,
}

/// v1: forbidden query operands are rejected, never rewritten (edit 1).
#[derive(Debug)]
#[non_exhaustive]
pub enum InputVerdict {
    Allow,
    Block { reason: String },
}

#[derive(Debug)]
pub struct Protected<T> {
    pub value: T,
    /// Count of values withheld/masked (audited; no values).
    pub withheld: u32,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtectError {
    /// Detector/classifier failed or unknown shape under active protection: fail closed.
    #[error("data protection unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait DataProtector: Send + Sync {
    /// SU default: true → hooks skipped → 6.2.0 bytes.
    fn is_passthrough(&self) -> bool;

    /// Selects and pins the rule version for this call.
    fn pin<'a>(
        &'a self,
        ctx: &'a RequestContext,
        subject: Subject,
        ops: Arc<OperationSet>,
        output: OutputClass,
    ) -> BoxFut<'a, Result<Arc<ProtectionContext>, ProtectError>>;

    /// Whole operation set (returned, written, predicate, ordering fields).
    fn guard_input<'a>(
        &'a self,
        p: &'a ProtectionContext,
        args: &'a serde_json::Value,
    ) -> BoxFut<'a, Result<InputVerdict, ProtectError>>;

    /// `Json`/`Schema` are not exempt; instance-controlled free text is scanned.
    fn protect_result<'a>(
        &'a self,
        p: &'a ProtectionContext,
        body: ResultBody,
    ) -> BoxFut<'a, Result<Protected<ResultBody>, ProtectError>>;

    /// Infallible: must redact or replace with fixed safe text.
    fn protect_error<'a>(
        &'a self,
        p: &'a ProtectionContext,
        err: ToolError,
    ) -> BoxFut<'a, ToolError>;

    /// `None` drops the message.
    fn protect_progress(&self, p: &ProtectionContext, message: String) -> Option<String>;

    fn protect_resource<'a>(
        &'a self,
        p: &'a ProtectionContext,
        body: serde_json::Value,
    ) -> BoxFut<'a, Result<Protected<serde_json::Value>, ProtectError>>;
}

/// The only progress sink a handler can reach after admission: every message passes
/// `protect_progress` under the call's pinned context. Built by the dispatcher at
/// admission (RFC §10.1 step 8). Attaching it to a context needs an
/// [`crate::context::AdmissionKey`].
pub struct ProtectedProgress {
    inner: Arc<dyn ProgressSink>,
    protector: Arc<dyn DataProtector>,
    pinned: Arc<ProtectionContext>,
}

impl ProtectedProgress {
    pub fn new(
        inner: Arc<dyn ProgressSink>,
        protector: Arc<dyn DataProtector>,
        pinned: Arc<ProtectionContext>,
    ) -> Self {
        Self {
            inner,
            protector,
            pinned,
        }
    }
}

impl ProgressSink for ProtectedProgress {
    fn emit(&self, progress: f64, total: Option<f64>, message: Option<String>) -> BoxFut<'_, ()> {
        let message = message.and_then(|m| self.protector.protect_progress(&self.pinned, m));
        self.inner.emit(progress, total, message)
    }
}
