//! Continuation boundary (review edit 7). Owned by hpm-protocol; tool handlers
//! never manage MRTR. rmcp 3.10.0 supplies the response shape only
//! (`handler/server.rs:593-598` `CallToolResponse`); its default `requestState`
//! sealing has no binding/expiry (`model/request_state.rs:446-447,540-549`).

use std::time::SystemTime;

use crate::BoxFut;
use crate::context::{Principal, ProtocolVersion};
use crate::operation::Subject;

/// Everything a resumed call must match. Contains no secrets or protected arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContinuationBinding {
    pub principal: Principal,
    pub client_id: String,
    /// `tools/call` or `resources/read`.
    pub method: &'static str,
    pub subject: Subject,
    /// Digest of canonical validated arguments.
    pub args_digest: [u8; 32],
    pub instance_id: Option<String>,
    pub config_version: u64,
    pub expires_at: SystemTime,
}

/// Opaque server-side single-use id (preferred over sealed client-held state).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContinuationHandle(pub String);

/// How a pre-execution `ConnectRequired` reaches the client, chosen from the
/// VERIFIED protocol version and capabilities.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ConnectPrompt {
    /// Legacy session with `elicitation.url` capability.
    LegacyUrlElicitation { url: String },
    /// Modern (`>= 2026-07-28`) `InputRequired` carrying the handle as request state.
    InputRequired {
        url: String,
        handle: ContinuationHandle,
    },
    /// No capability: plain text containing the connect link (6.2.0 behaviour).
    ConnectLink { url: String },
}

pub fn select_prompt(
    protocol: &ProtocolVersion,
    caps: &serde_json::Value,
    url: String,
    handle: ContinuationHandle,
) -> ConnectPrompt {
    let has_url_elicitation = caps.pointer("/elicitation/url").is_some();
    if protocol.supports_mrtr() {
        ConnectPrompt::InputRequired { url, handle }
    } else if has_url_elicitation {
        ConnectPrompt::LegacyUrlElicitation { url }
    } else {
        ConnectPrompt::ConnectLink { url }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ContinuationError {
    #[error("continuation expired")]
    Expired,
    #[error("continuation already redeemed")]
    AlreadyRedeemed,
    /// Principal/client/tool/args/target/config changed ⇒ fresh admission.
    #[error("continuation binding mismatch")]
    BindingMismatch,
    #[error("continuation store unavailable")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait ContinuationStore: Send + Sync {
    fn issue<'a>(
        &'a self,
        binding: ContinuationBinding,
    ) -> BoxFut<'a, Result<ContinuationHandle, ContinuationError>>;
    /// Atomic single-use redemption across replicas, BEFORE mutation admission.
    /// After success the call re-runs authentication, authorization and
    /// credential preparation; it never resumes after an admitted mutation.
    fn redeem<'a>(
        &'a self,
        handle: &'a ContinuationHandle,
        presented: &'a ContinuationBinding,
    ) -> BoxFut<'a, Result<(), ContinuationError>>;
}
