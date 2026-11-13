//! MCP protocol adapters (plan §5.1). Each adapter maps rmcp's request context into
//! trusted admission (`TrustedRoot::admit`, RFC §10.1 step 0) and hands the admitted
//! call to the shared `hpm_tools::dispatcher::Dispatcher`. stdout carries only
//! JSON-RPC in stdio mode (C10).
//!
//! Lane E productionizes `spikes/s1-transport` (C1–C11) into [`transport`].

mod handler;
pub mod stdio;
pub mod transport;

pub use handler::McpHandler;
