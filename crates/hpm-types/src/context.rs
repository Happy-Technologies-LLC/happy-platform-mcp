//! RequestContext (RFC §2): resolved once per call (F5), immutable afterwards.
//!
//! Fields are private. Only a `TrustedRoot` (constructible with feature `trusted-root`,
//! composition root only) builds a context, and attaching the bound credential and
//! pinned protection context at admission needs an [`AdmissionKey`] minted by that
//! root (review edits 3, 4, 6).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Single-user HTTP (Streamable HTTP or legacy SSE) authenticated by the static
    /// bearer token. Distinct from `LocalOperator` so audit separates stdio from HTTP;
    /// sessions bind to `token_fingerprint` (SHA-256 of the token, threat model
    /// T-B01). Lead decision 2026-11-13 (RFC §14), pending owner review.
    LocalBearer { token_fingerprint: [u8; 32] },
    /// Multi-user HTTP only.
    Federated {
        org: String,
        iss: String,
        sub: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentity {
    /// OAuth client_id (enterprise) or `clientInfo.name` (single-user; untrusted, audit only).
    pub client_id: String,
    pub verified: bool,
}

/// Physical transport. Not a protocol version (review edit 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportKind {
    Stdio,
    /// Hand-written HTTP+SSE adapter. One SSE stream per session.
    LegacySse,
    /// rmcp Streamable HTTP (stateless modern and legacy session paths).
    StreamableHttp,
}

/// Exact negotiated MCP protocol version string, e.g. `2026-07-28`, `2025-06-18`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProtocolVersion(String);

impl ProtocolVersion {
    /// First protocol version with multi-round-trip requests (MRTR, C14).
    pub const MRTR_MIN: &'static str = "2026-07-28";

    pub fn new(v: impl Into<String>) -> Self {
        Self(v.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// MCP versions are ISO dates, which compare lexicographically.
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
/// default mid-call; it changes what the next call resolves (per principal in MU).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedInstance {
    /// Stable id (survives rename); not the URL, not a hash of the mutable name.
    pub instance_id: String,
    pub canonical_url: String,
    pub config_version: u64,
}

/// Server-generated, globally unique call id (UUIDv7). The MCP request id is a
/// separate correlation field (review edit 5).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CallId(String);

impl CallId {
    fn generate() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Progress sink bound to one request (C8 as scoped in RFC §2.1). After admission,
/// handlers only reach the protected sink via [`RequestContext::progress`].
pub trait ProgressSink: Send + Sync {
    fn emit(&self, progress: f64, total: Option<f64>, message: Option<String>) -> BoxFut<'_, ()>;
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
    /// Set once at admission. Handlers and UI-session acquisition consume this
    /// binding; they never re-resolve (edit 4).
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
    pub fn deadline(&self) -> Instant {
        self.deadline
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

    /// Dispatcher step 8 (RFC §10.1): attach the admission products and return a new
    /// immutable context. Requires the [`AdmissionKey`] the composition root minted.
    pub fn admitted(
        &self,
        _key: &AdmissionKey,
        credential: Option<CredentialBinding>,
        protection: Option<Arc<ProtectionContext>>,
        progress: Arc<dyn ProgressSink>,
    ) -> RequestContext {
        RequestContext {
            credential,
            protection,
            progress,
            ..self.clone()
        }
    }
}

/// Identity verified by the Tower auth layer (JWT/introspection), read back from
/// `http::request::Parts` extensions by the HTTP adapters.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct VerifiedHttpIdentity {
    pub principal: Principal,
    pub client: ClientIdentity,
    pub scopes: Arc<[String]>,
}

impl VerifiedHttpIdentity {
    pub fn new(principal: Principal, client: ClientIdentity, scopes: Arc<[String]>) -> Self {
        Self {
            principal,
            client,
            scopes,
        }
    }
}

/// Per-request inputs the adapter extracts from the transport. None of them can
/// grant trust.
pub struct AdmissionInput {
    pub mcp_request_id: String,
    pub protocol_version: ProtocolVersion,
    pub client_capabilities: serde_json::Value,
    /// Unverified `clientInfo.name` (stdio audit label only).
    pub client_name: String,
    pub instance: Option<ResolvedInstance>,
    /// Child of the transport's request cancellation token.
    pub cancel: CancellationToken,
    pub deadline: Instant,
    pub progress: Arc<dyn ProgressSink>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdmissionError {
    /// An HTTP route reached the handler without verified identity: the auth layer
    /// is misconfigured. Fails closed in both deployment modes.
    #[error("verified identity missing on HTTP route")]
    MissingVerifiedIdentity,
    #[error("stdio adapter received an HTTP identity")]
    UnexpectedIdentity,
    /// The verified principal does not fit the listener: `LocalOperator` on HTTP,
    /// `LocalBearer` outside single-user HTTP, or `Federated` outside multi-user HTTP.
    #[error("principal not allowed on this listener")]
    PrincipalNotAllowed,
}

/// One-shot latch. True only for the first caller in the process.
#[cfg_attr(not(feature = "trusted-root"), allow(dead_code))]
fn first_take(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::SeqCst)
}

/// Capability to attach admission products ([`RequestContext::admitted`]). Only the
/// composition root can mint one (feature `trusted-root`) and hands it to the
/// dispatcher; lane crates compile without the feature and cannot create it.
#[derive(Clone, Copy, Debug)]
pub struct AdmissionKey(());

#[cfg(feature = "trusted-root")]
static KEY_MINTED: AtomicBool = AtomicBool::new(false);

impl AdmissionKey {
    /// Succeeds at most once per process (runtime backstop to the feature gate and
    /// the `xtask check-trusted-root` CI gate): a forged second mint returns `None`.
    /// Test binaries that build several dispatchers share one key through a
    /// `static KEY: LazyLock<AdmissionKey>`.
    #[cfg(feature = "trusted-root")]
    pub fn mint() -> Option<Self> {
        first_take(&KEY_MINTED).then_some(Self(()))
    }
}

/// Trusted admission for one listener. Transport and deployment mode come from here,
/// never from the presence or absence of request extensions (edit 6). Constructors
/// need feature `trusted-root` (composition root only); a protocol adapter receives a
/// built `TrustedRoot` and calls [`TrustedRoot::admit`].
#[derive(Debug)]
pub struct TrustedRoot {
    mode: DeploymentMode,
    transport: TransportKind,
}

/// Not latched: tests (lane E) build several roots per process, and a root grants
/// no authority to attach admission products without the single-mint
/// [`AdmissionKey`]. The feature gate and the CI gate keep them in hpm-cli.
#[cfg(feature = "trusted-root")]
impl TrustedRoot {
    /// The only path to `Principal::LocalOperator`. Single-user by construction.
    pub fn single_user_stdio() -> Self {
        Self {
            mode: DeploymentMode::SingleUser,
            transport: TransportKind::Stdio,
        }
    }
    pub fn streamable_http(mode: DeploymentMode) -> Self {
        Self {
            mode,
            transport: TransportKind::StreamableHttp,
        }
    }
    pub fn legacy_sse(mode: DeploymentMode) -> Self {
        Self {
            mode,
            transport: TransportKind::LegacySse,
        }
    }
}

impl TrustedRoot {
    pub fn mode(&self) -> DeploymentMode {
        self.mode
    }
    pub fn transport(&self) -> TransportKind {
        self.transport
    }

    /// Dispatcher step 0 (RFC §10.1). `http_identity` is `None` on stdio and the
    /// verified identity from request extensions on every HTTP route.
    pub fn admit(
        &self,
        input: AdmissionInput,
        http_identity: Option<VerifiedHttpIdentity>,
    ) -> Result<RequestContext, AdmissionError> {
        let (principal, client, scopes) = match (self.transport, http_identity) {
            (TransportKind::Stdio, None) => (
                Principal::LocalOperator,
                ClientIdentity {
                    client_id: input.client_name,
                    verified: false,
                },
                Arc::from(Vec::<String>::new()),
            ),
            (TransportKind::Stdio, Some(_)) => return Err(AdmissionError::UnexpectedIdentity),
            // HTTP: the principal must match the deployment mode (never LocalOperator).
            (_, Some(id)) => {
                let allowed = matches!(
                    (&id.principal, self.mode),
                    (Principal::LocalBearer { .. }, DeploymentMode::SingleUser)
                        | (Principal::Federated { .. }, DeploymentMode::MultiUser)
                );
                if !allowed {
                    return Err(AdmissionError::PrincipalNotAllowed);
                }
                (id.principal, id.client, id.scopes)
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    struct NoProgress;
    impl ProgressSink for NoProgress {
        fn emit(&self, _: f64, _: Option<f64>, _: Option<String>) -> BoxFut<'_, ()> {
            Box::pin(async {})
        }
    }

    fn input() -> AdmissionInput {
        AdmissionInput {
            mcp_request_id: "1".into(),
            protocol_version: ProtocolVersion::new("2025-06-18"),
            client_capabilities: serde_json::Value::Null,
            client_name: "t".into(),
            instance: None,
            cancel: CancellationToken::new(),
            deadline: Instant::now(),
            progress: Arc::new(NoProgress),
        }
    }

    fn id(principal: Principal) -> Option<VerifiedHttpIdentity> {
        Some(VerifiedHttpIdentity::new(
            principal,
            ClientIdentity {
                client_id: "c".into(),
                verified: true,
            },
            Arc::from(Vec::<String>::new()),
        ))
    }

    #[test]
    fn mint_latch_succeeds_once() {
        let flag = AtomicBool::new(false);
        assert!(first_take(&flag));
        assert!(!first_take(&flag));
        assert!(!first_take(&flag));
    }

    #[test]
    fn admit_binds_principal_to_transport_and_mode() {
        use DeploymentMode::{MultiUser, SingleUser};
        use TransportKind::{LegacySse, Stdio, StreamableHttp};
        let local = || Principal::LocalOperator;
        let bearer = || Principal::LocalBearer {
            token_fingerprint: [7; 32],
        };
        let fed = || Principal::Federated {
            org: "o".into(),
            iss: "i".into(),
            sub: "s".into(),
        };
        type Case = (
            TransportKind,
            DeploymentMode,
            Option<Principal>,
            Option<Principal>,
        );
        let cases: Vec<Case> = vec![
            // stdio: only LocalOperator, and only without an HTTP identity.
            (Stdio, SingleUser, None, Some(local())),
            (Stdio, SingleUser, Some(local()), None),
            (Stdio, SingleUser, Some(bearer()), None),
            // single-user HTTP: only LocalBearer.
            (StreamableHttp, SingleUser, Some(bearer()), Some(bearer())),
            (LegacySse, SingleUser, Some(bearer()), Some(bearer())),
            (StreamableHttp, SingleUser, Some(local()), None),
            (LegacySse, SingleUser, Some(local()), None),
            (StreamableHttp, SingleUser, Some(fed()), None),
            // multi-user HTTP: only Federated.
            (StreamableHttp, MultiUser, Some(fed()), Some(fed())),
            (LegacySse, MultiUser, Some(fed()), Some(fed())),
            (StreamableHttp, MultiUser, Some(local()), None),
            (StreamableHttp, MultiUser, Some(bearer()), None),
            // HTTP without verified identity fails closed.
            (StreamableHttp, SingleUser, None, None),
            (StreamableHttp, MultiUser, None, None),
        ];
        for (transport, mode, given, expected) in cases {
            let root = TrustedRoot { mode, transport };
            let got = root
                .admit(input(), given.clone().and_then(id))
                .ok()
                .map(|c| c.principal().clone());
            assert_eq!(got, expected, "{transport:?} {mode:?} {given:?}");
        }
    }
}
