//! Dispatcher boundary types (RFC §10). The pipeline itself (validate → classify →
//! normalize + floor → script guard → policy → protection pin + input guard → bound
//! credential → bind context → liveness → durable attempt → handler → protect →
//! finalize audit → safe render) is implemented by the dispatcher crate; this module
//! holds only what crosses the protocol-adapter boundary.

use crate::tool::ToolDescriptor;

/// Validates arguments against the exact public `inputSchema` (C6).
pub trait SchemaValidator: Send + Sync {
    /// Error text must not echo argument values.
    fn validate(&self, tool: &ToolDescriptor, args: &serde_json::Value) -> Result<(), String>;
}

/// Text sent to the client (6.2.0: one text content item plus `isError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub is_error: bool,
}

/// The protocol adapter maps `ConnectRequired` to a continuation and a
/// `ConnectPrompt` (RFC §4.1). Nothing has been admitted when it is returned.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DispatchOutcome {
    Rendered(Rendered),
    ConnectRequired { connect_url: String },
}

/// Stable reason code for a rejected `javascript:` operand (RFC §3.2).
pub const QUERY_SCRIPT_NOT_ALLOWED: &str = "QUERY_SCRIPT_NOT_ALLOWED";
/// Stable reason code for an unknown table class under active policy or protection.
pub const UNCLASSIFIED_TABLE: &str = "UNCLASSIFIED_TABLE";
/// Stable reason code for an effective Execute operation under active protection
/// whose pinned `on_unanalyzable` is `Deny` (RFC §13 Q2, D-06).
pub const EXECUTE_UNDER_PROTECTION: &str = "EXECUTE_UNDER_PROTECTION";
