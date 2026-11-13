//! PROVISIONAL (P0, rev v0.2 2026-11-13). Sketch of the `hpm-types` contracts from plan §5.2.
//! Freezes in P2 (R1) after P1 spikes report. See /tmp/hpm-specs/p0/contract-rfc.md.
//!
//! No rmcp dependency on purpose: contracts are transport-agnostic; `hpm-protocol`
//! maps rmcp's `RequestContext<RoleServer>` into [`context::RequestContext`].

pub mod audit;
pub mod context;
pub mod continuation;
pub mod credential;
pub mod dispatch;
pub mod operation;
pub mod policy;
pub mod protect;
pub mod registry;
pub mod tool;

use std::pin::Pin;

/// Object-safe async return type (avoids an `async-trait` dependency; traits are used as `dyn`).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
