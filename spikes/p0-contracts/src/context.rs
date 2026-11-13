//! RequestContext: resolved ONCE per call (F5), immutable afterwards.
//!
//! Fields are private. Only the trusted composition root (feature `trusted-root`)
//! can build a context, and only the dispatcher (crate-private) can attach the
//! bound credential and pinned protection context at admission (review edits 3, 4, 6).

use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::BoxFut;
use crate::credential::CredentialBinding;
use crate::protect::ProtectionContext;

/// Stable identity. Enterprise keys on `(org, iss, sub)` (F3); never on email.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Principal {
    /// Single-user stdio only, created by the explicit stdio adapter. Never inferred
    /// from a missing HTTP extension.
    LocalOperator,
    Federated { org: String, iss: String, sub: String },
}

#[derive(Clone, Debug)]
pub struct ClientIdentity {
    /// OAuth client_id (enterprise) or `clientInfo.name` (single-user; untrusted, audit only).
    pub client_id: String,
    pub verified: bool,
}

/// Physical transport. NOT a protocol version (review edit 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportKind {
    Stdio,
    /// Hand-written HTTP+SSE adapter (D1). One SSE stream per session.
    LegacySse,
    /// rmcp Streamable HTTP (stateless modern and legacy session paths).
    StreamableHttp,
}

/// Exact negotiated MCP protocol version string, e.g. `2026-07-28`, `2025-06-18`.
/// From rmcp `ctx.meta.protocol_version()` (modern, rmcp-3.10.0 `model/meta.rs:449`)
/// or the initialize result (legacy).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolVersion(String);
impl ProtocolVersion {
    pub const MRTR_MIN: &'static str = "2026-07-28";
    pub fn new(v: impl Into<String>) -> Self {
        Self(v.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// ISO dates compare lexicographically.
    pub fn supports_mrtr(&self) -> bool {
        self.0.as_str() >= Self::MRTR_MIN
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeploymentMode {
    SingleUser,
    MultiUser,
}

/// Instance resolved from one `RegistryView`. `SN-Set-Instance` never mutates a
/// shared default mid-call; it changes what the NEXT call resolves (per principal in MU).
#[derive(Clone, Debug)]
pub struct ResolvedInstance {
    /// Stable id (survives rename); not the URL, not a hash of the mutable name.
    pub instance_id: String,
    pub canonical_url: String,
    pub config_version: u64,
}

/// Server-generated, globally unique call id. The MCP request id is a separate
/// correlation field (review edit 5).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CallId(String);
impl CallId {
    #[cfg(feature = "trusted-root")]
    pub(crate) fn generate() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        // Sketch only: R1 uses UUIDv7. Process-unique counter + wall-clock nanos.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        Self(format!("{t:x}-{n:x}"))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Raw progress sink bound to the request (C8 as scoped in RFC §2.1). Not
/// reachable by handlers directly: handlers get the protected sink installed at
/// admission via [`RequestContext::progress`].
pub trait ProgressSink: Send + Sync {
    fn emit(&self, progress: f64, total: Option<f64>, message: Option<String>) -> BoxFut<'_, ()>;
}

pub struct NoProgress;
impl ProgressSink for NoProgress {
    fn emit(&self, _: f64, _: Option<f64>, _: Option<String>) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
}

#[derive(Clone)]
pub struct RequestContext {
    call_id: CallId,
    mcp_request_id: String,
    principal: Principal,
    client: ClientIdentity,
    scopes: Arc<[String]>,
    transport: TransportKind,
    protocol_version: ProtocolVersion,
    client_capabilities: serde_json::Value,
    mode: DeploymentMode,
    instance: Option<ResolvedInstance>,
    cancel: CancellationToken,
    deadline: Instant,
    progress: Arc<dyn ProgressSink>,
    /// Set once at admission by the dispatcher (edit 4). Handler and UI-session
    /// acquisition consume THIS binding; they never re-resolve.
    credential: Option<CredentialBinding>,
    /// Set once at admission (edit 3).
    protection: Option<Arc<ProtectionContext>>,
}

impl RequestContext {
    pub fn call_id(&self) -> &CallId {
        &self.call_id
    }
    pub fn mcp_request_id(&self) -> &str {
        &self.mcp_request_id
    }
    pub fn principal(&self) -> &Principal {
        &self.principal
    }
    pub fn client(&self) -> &ClientIdentity {
        &self.client
    }
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }
    pub fn transport(&self) -> TransportKind {
        self.transport
    }
    pub fn protocol_version(&self) -> &ProtocolVersion {
        &self.protocol_version
    }
    pub fn client_capabilities(&self) -> &serde_json::Value {
        &self.client_capabilities
    }
    pub fn mode(&self) -> DeploymentMode {
        self.mode
    }
    pub fn instance(&self) -> Option<&ResolvedInstance> {
        self.instance.as_ref()
    }
    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }
    pub fn credential(&self) -> Option<&CredentialBinding> {
        self.credential.as_ref()
    }
    pub fn protection(&self) -> Option<&Arc<ProtectionContext>> {
        self.protection.as_ref()
    }
    /// After admission this is the protected request-bound sink (edit 3).
    pub fn progress(&self) -> &Arc<dyn ProgressSink> {
        &self.progress
    }
    pub fn is_live(&self) -> bool {
        !self.cancel.is_cancelled() && Instant::now() < self.deadline
    }

    /// Dispatcher-only: attach the admission products. Returns a new immutable context.
    pub(crate) fn admitted(
        &self,
        credential: Option<CredentialBinding>,
        protection: Option<Arc<ProtectionContext>>,
        progress: Arc<dyn ProgressSink>,
    ) -> RequestContext {
        RequestContext { credential, protection, progress, ..self.clone() }
    }
}

/// Identity verified by the Tower auth layer (JWT/introspection) and read back
/// from `http::request::Parts` extensions (rmcp-3.10.0 `transport/streamable_http_server/tower.rs:1679,2426`).
#[derive(Clone, Debug)]
pub struct VerifiedHttpIdentity {
    pub principal: Principal,
    pub client: ClientIdentity,
    pub scopes: Arc<[String]>,
}

/// Per-request inputs the adapter extracts from rmcp. None of them can grant trust.
pub struct AdmissionInput {
    pub mcp_request_id: String,
    pub protocol_version: ProtocolVersion,
    pub client_capabilities: serde_json::Value,
    /// Unverified `clientInfo.name` (stdio audit label only).
    pub client_name: String,
    pub instance: Option<ResolvedInstance>,
    /// Child of rmcp `ctx.ct`.
    pub cancel: CancellationToken,
    pub deadline: Instant,
    pub progress: Arc<dyn ProgressSink>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdmissionError {
    /// HTTP route reached the handler without verified identity: auth layer
    /// misconfigured. Fails closed in BOTH deployment modes.
    #[error("verified identity missing on HTTP route")]
    MissingVerifiedIdentity,
    #[error("stdio adapter received an HTTP identity")]
    UnexpectedIdentity,
}

/// Trusted composition root. Transport and deployment mode come from here,
/// never from the presence/absence of request extensions (edit 6).
#[cfg(feature = "trusted-root")]
pub struct TrustedRoot {
    mode: DeploymentMode,
    transport: TransportKind,
}

#[cfg(feature = "trusted-root")]
impl TrustedRoot {
    /// The ONLY path to `Principal::LocalOperator`. Single-user by construction.
    pub fn single_user_stdio() -> Self {
        Self { mode: DeploymentMode::SingleUser, transport: TransportKind::Stdio }
    }
    pub fn streamable_http(mode: DeploymentMode) -> Self {
        Self { mode, transport: TransportKind::StreamableHttp }
    }
    pub fn legacy_sse(mode: DeploymentMode) -> Self {
        Self { mode, transport: TransportKind::LegacySse }
    }

    pub fn admit(
        &self,
        input: AdmissionInput,
        http_identity: Option<VerifiedHttpIdentity>,
    ) -> Result<RequestContext, AdmissionError> {
        let (principal, client, scopes) = match (self.transport, http_identity) {
            (TransportKind::Stdio, None) => (
                Principal::LocalOperator,
                ClientIdentity { client_id: input.client_name, verified: false },
                Arc::from(Vec::<String>::new()),
            ),
            (TransportKind::Stdio, Some(_)) => return Err(AdmissionError::UnexpectedIdentity),
            (_, Some(id)) => (id.principal, id.client, id.scopes),
            (_, None) => return Err(AdmissionError::MissingVerifiedIdentity),
        };
        Ok(RequestContext {
            call_id: CallId::generate(),
            mcp_request_id: input.mcp_request_id,
            principal,
            client,
            scopes,
            transport: self.transport,
            protocol_version: input.protocol_version,
            client_capabilities: input.client_capabilities,
            mode: self.mode,
            instance: input.instance,
            cancel: input.cancel,
            deadline: input.deadline,
            progress: input.progress,
            credential: None,
            protection: None,
        })
    }
}
