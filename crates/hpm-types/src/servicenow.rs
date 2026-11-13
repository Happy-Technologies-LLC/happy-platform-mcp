//! ServiceNowAccess (W3 baseline G2): how a handler reaches ServiceNow.
//!
//! A handler never picks an instance or a client itself. It asks for the client
//! pinned to the call's [`CredentialBinding`](crate::credential::CredentialBinding),
//! which `CredentialProvider::prepare` fixed at admission from
//! `RequestContext::instance()` (the per-call `instance` argument, else the selected
//! default). The handle is pinned to that binding's instance and credential
//! generation: every request through it, multi-step flows included, runs on that
//! generation's URL and credentials and fails closed (stale, `Error: Instance
//! changed during credential resolution`) once the instance was reconfigured
//! (same instance id, new registry entry), so no step can land on another
//! instance (W2-B L-C). `SN-Set-Instance` only changes the default selection for
//! later calls; it never moves an in-flight call's binding.

use crate::BoxFut;
use crate::context::RequestContext;
use crate::tool::ToolError;

pub trait ServiceNowAccess: Send + Sync {
    /// The pinned client handle (single-user: `hpm_core::client::PinnedClient`).
    type Client: Send + Sync;

    /// The client pinned to `ctx.credential()`. Errors:
    /// `ToolError::Credential(NoInstance)` when the call has no binding (the tool
    /// declared `CredentialNeed::None`), `ToolError::Credential(GenerationMoved)` when
    /// the bound instance or generation is no longer current.
    fn client<'a>(&'a self, ctx: &'a RequestContext)
    -> BoxFut<'a, Result<Self::Client, ToolError>>;
}
