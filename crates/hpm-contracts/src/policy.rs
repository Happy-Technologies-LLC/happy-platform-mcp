//! PolicyEngine: decides normalized operations (F1) for tools AND resources (edit 9).
//! Own small declarative rules (Q3): default deny, deny precedence, whole-call evaluation.

use crate::BoxFut;
use crate::context::RequestContext;
use crate::operation::{OperationSet, Subject};

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Decision {
    Allow,
    /// `reason` is a stable code shown to the user and audited; no protected values.
    Deny {
        reason: String,
    },
}

#[derive(Clone, Debug)]
pub struct PolicyDecision {
    pub decision: Decision,
    pub policy_version: u64,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyError {
    /// Always treated as Deny (fail closed).
    #[error("policy unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait PolicyEngine: Send + Sync {
    /// Whole OperationSet in one decision; any denied element denies the call.
    /// Under active protection, effective Execute ops are denied unless an explicit,
    /// scoped, audited exception applies (Q2).
    fn decide<'a>(
        &'a self,
        ctx: &'a RequestContext,
        ops: &'a OperationSet,
    ) -> BoxFut<'a, Result<PolicyDecision, PolicyError>>;
    /// Listing filter only (tools/list, resources/list). Never authorization.
    fn visible<'a>(&'a self, ctx: &'a RequestContext, subject: &'a Subject) -> BoxFut<'a, bool>;
    /// True when rules apply (anything other than single-user allow-all). Unknown
    /// table classification fails closed only when policy or protection applies.
    fn enforces(&self) -> bool;
}
