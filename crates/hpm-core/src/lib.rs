//! Core services of happy-platform-mcp (plan §5.1). Implements the hpm-types
//! contracts that touch configuration, secrets and ServiceNow.
//!
//! W2 lanes (`/tmp/hpm-specs/p2/w2-lanes.md`): lane A owns `config`, `registry`,
//! `keychain` and `token_store`; lane B owns `client`.

pub mod client;
pub mod config;
pub mod credential;
pub mod keychain;
pub mod registry;
pub mod token_store;
