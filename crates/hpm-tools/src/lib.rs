//! Tools of happy-platform-mcp (plan §5.1).
//!
//! - [`dispatcher`]: the one pipeline every `tools/call` and `resources/read` runs
//!   through, on stdio and HTTP alike (plan §5.2 Placement, RFC v0.2 §10.1). Owned
//!   by the lead.
//! - [`registration`]: how tool lanes add descriptors.
//! - [`single_user`]: the single-user service implementations wired by default.
//! - [`families`]: one tool family per W3 lane.
//! - [`table_metadata`]: the bundled 6.2.0 table definitions.
//! - [`logic`]: pure-logic ports with no I/O (lane C).

pub mod dispatcher;
pub mod families;
pub mod logic;
pub mod registration;
pub mod schema;
pub mod single_user;
pub mod table_metadata;
