//! MCP protocol adapters (plan §5.1). Each adapter maps rmcp's request context into
//! trusted admission (`TrustedRoot::admit`, RFC §10.1 step 0) and hands the admitted
//! call to the shared `hpm_tools::dispatcher::Dispatcher`. stdout carries only
//! JSON-RPC in stdio mode (C10). All rmcp use stays inside this crate.

pub mod continuation;
mod handler;
pub mod stdio;
pub mod transport;

pub use handler::{LIST_TTL_MS, McpHandler, McpHandlerBuilder, SERVER_NAME};
