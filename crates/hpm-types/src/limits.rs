//! Admission limits (public seam PS-6, shared with PS-7).
//!
//! The dispatcher consults an [`AdmissionLimiter`] once per tool call and resource
//! read, after the policy `Allow` and before protection pinning and `begin_attempt`.
//! The returned [`AdmissionPermit`] lives until the call has been rendered.
//! Without a limiter every call is admitted with [`AdmissionPermit::unlimited`].

use std::time::Duration;

use crate::context::RequestContext;
use crate::operation::OperationSet;

/// What is being admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionKind {
    ToolCall,
    ResourceRead,
}

/// One admission request. Build with [`Admission::new`].
#[non_exhaustive]
pub struct Admission<'a> {
    pub ctx: &'a RequestContext,
    /// Registered tool name; `""` for resource reads.
    pub tool: &'a str,
    /// Normalized operations (`has_execute()`, `mutates()`).
    pub ops: &'a OperationSet,
    pub kind: AdmissionKind,
}

impl<'a> Admission<'a> {
    pub fn new(
        ctx: &'a RequestContext,
        tool: &'a str,
        ops: &'a OperationSet,
        kind: AdmissionKind,
    ) -> Self {
        Self {
            ctx,
            tool,
            ops,
            kind,
        }
    }
}

/// RAII: dropping it releases the in-flight slots. Opaque to the dispatcher.
pub struct AdmissionPermit(#[allow(dead_code)] Box<dyn std::any::Any + Send + Sync>);

impl AdmissionPermit {
    /// Wraps a guard whose `Drop` releases the limiter's slots.
    pub fn new<T: Send + Sync + 'static>(guard: T) -> Self {
        Self(Box::new(guard))
    }

    /// A permit that holds nothing.
    pub fn unlimited() -> Self {
        Self(Box::new(()))
    }
}

impl std::fmt::Debug for AdmissionPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdmissionPermit")
    }
}

/// A refused admission. `code` is the audit event kind, `scope` the client-visible
/// scope word; both are static strings.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LimitExceeded {
    /// `"limit_inflight"`, `"limit_rate"`, `"limit_execute_inflight"`,
    /// `"limit_execute_rate"`, `"limit_sessions"`, `"limit_streams"`.
    pub code: &'static str,
    /// `"inflight"` | `"rate"` | `"execute_inflight"` | `"execute_rate"` |
    /// `"sessions"` | `"streams"`.
    pub scope: &'static str,
    pub retry_after: Option<Duration>,
}

impl LimitExceeded {
    pub fn new(code: &'static str, scope: &'static str, retry_after: Option<Duration>) -> Self {
        Self {
            code,
            scope,
            retry_after,
        }
    }
}

pub trait AdmissionLimiter: Send + Sync {
    /// Called once per tool call and resource read, after the policy Allow and
    /// before `begin_attempt`. Synchronous and non-blocking: refuse, never wait.
    fn admit(&self, req: Admission<'_>) -> Result<AdmissionPermit, LimitExceeded>;
}

/// Client-visible text of `ToolError::RateLimited`: `RATE_LIMITED: <scope>` plus
/// ` (retry after N s)` when the wait is known.
pub fn rate_limited_text(scope: &str, retry_after_secs: Option<u32>) -> String {
    match retry_after_secs {
        Some(n) => format!("RATE_LIMITED: {scope} (retry after {n} s)"),
        None => format!("RATE_LIMITED: {scope}"),
    }
}
