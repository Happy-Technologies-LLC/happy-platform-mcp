//! CredentialProvider: one prepared, bound credential per call (review edit 4).

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use zeroize::Zeroizing;

use crate::BoxFut;
use crate::context::{Principal, RequestContext};

/// Redacting, zeroizing wrapper (non-blocking suggestion 4). No Display/Serialize;
/// Debug prints a placeholder; memory cleared on drop.
pub struct Secret(Zeroizing<String>);
impl Secret {
    pub fn new(s: String) -> Self {
        Self(Zeroizing::new(s))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// What a call needs prepared before admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialNeed {
    /// Local/config-only tools: no ServiceNow credential.
    None,
    Rest,
    /// REST + a UI session (`/sys.scripts.do`, pickers).
    RestAndUiSession,
}

#[derive(Debug)]
struct BindingInner {
    principal: Principal,
    instance_id: String,
    config_version: u64,
    generation: u64,
    bound_sys_id: Option<String>,
    /// Provider-private vault/keychain key. Not a secret.
    handle: String,
}

/// Fixes principal, instance/config binding, credential generation and verified
/// downstream identity for ONE call. Created only by `CredentialProvider::prepare`.
#[derive(Clone, Debug)]
pub struct CredentialBinding(Arc<BindingInner>);
impl CredentialBinding {
    /// For provider implementations.
    pub fn new(
        principal: Principal,
        instance_id: String,
        config_version: u64,
        generation: u64,
        bound_sys_id: Option<String>,
        handle: String,
    ) -> Self {
        Self(Arc::new(BindingInner { principal, instance_id, config_version, generation, bound_sys_id, handle }))
    }
    pub fn principal(&self) -> &Principal {
        &self.0.principal
    }
    pub fn instance_id(&self) -> &str {
        &self.0.instance_id
    }
    pub fn config_version(&self) -> u64 {
        self.0.config_version
    }
    pub fn generation(&self) -> u64 {
        self.0.generation
    }
    /// Verified ServiceNow sys_id (who-am-I, F3); None in SU.
    pub fn bound_sys_id(&self) -> Option<&str> {
        self.0.bound_sys_id.as_deref()
    }
    pub fn handle(&self) -> &str {
        &self.0.handle
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum CredentialKind {
    /// Full `Authorization` header value (Basic / Bearer).
    Authorization(Secret),
}

#[derive(Debug)]
pub struct Credential {
    pub kind: CredentialKind,
    /// Always equals the binding's generation.
    pub generation: u64,
    pub expires_at: Option<SystemTime>,
}

/// Lifetime of a ServiceNow UI session cookie jar. S4 (s4-bearer-report.md):
/// sessions outlive OAuth token revocation, so script runs log out per operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UiSessionScope {
    /// Fresh jar, `GET /logout.do` in the same jar when the operation ends
    /// (success, error or cancel). Required for `/sys.scripts.do`.
    PerOperation,
    /// Read-only pickers: pooled per (principal, instance, generation), logged out
    /// on sign-out, revocation, generation change and idle.
    PooledReadOnly,
}

/// Never leaves hpm-core; never persisted; never shared across principals.
#[derive(Debug)]
pub struct UiSession {
    pub cookie_header: Secret,
    pub csrf_token: Secret,
    pub generation: u64,
    pub scope: UiSessionScope,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CredentialError {
    /// Raised only by `prepare`, before admission. hpm-protocol maps it to a
    /// continuation (see `continuation.rs`); handlers never see it.
    #[error("connect required")]
    ConnectRequired { connect_url: String },
    #[error("credential revoked")]
    Revoked,
    /// Store moved past the bound generation (refresh/revoke elsewhere). The
    /// admitted call fails; it never silently switches to the new generation.
    #[error("credential generation changed")]
    GenerationMoved,
    #[error("no instance bound to this call")]
    NoInstance,
    #[error("ui session unsupported for this auth mode")]
    UiSessionUnsupported,
    #[error("credential backend unavailable")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait CredentialProvider: Send + Sync {
    /// After validation, authorization and input guards. Completes any interactive
    /// auth and any permitted refresh, checks revocation (MU: every call), verifies
    /// downstream identity, and returns the binding — or `ConnectRequired`. No
    /// business mutation has happened yet. `None` need ⇒ `Ok(None)`.
    fn prepare<'a>(&'a self, ctx: &'a RequestContext, need: CredentialNeed)
        -> BoxFut<'a, Result<Option<CredentialBinding>, CredentialError>>;
    /// Material for the bound generation only. Never re-resolves a default
    /// instance, never prompts, never refreshes into a new generation.
    fn authorize<'a>(&'a self, binding: &'a CredentialBinding) -> BoxFut<'a, Result<Credential, CredentialError>>;
    /// After a 401 with the bound generation. Fallible: storage failure fails closed.
    fn invalidate<'a>(&'a self, binding: &'a CredentialBinding) -> BoxFut<'a, Result<(), CredentialError>>;
    fn ui_session<'a>(&'a self, binding: &'a CredentialBinding, scope: UiSessionScope)
        -> BoxFut<'a, Result<UiSession, CredentialError>>;
    /// `GET /logout.do` in the session's jar. Mandatory for `PerOperation` on every
    /// exit path; errors are audited, never retried as the business operation.
    fn end_ui_session<'a>(&'a self, session: UiSession) -> BoxFut<'a, Result<(), CredentialError>>;
}
