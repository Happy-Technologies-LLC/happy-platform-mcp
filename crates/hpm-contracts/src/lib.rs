//! Contracts of happy-platform-mcp (plan §5.2; contract RFC v0.2, §13 answers signed
//! 2026-11-13).
//!
//! Types and traits only. Implementations (single-user and enterprise) live in other
//! crates and are wired by the composition root. There is no `rmcp` dependency on
//! purpose: contracts stay transport-agnostic and testable without a transport; the
//! protocol adapter maps rmcp's request context into [`context::RequestContext`].
//!
//! The `trusted-root` feature gates construction of request contexts and admission
//! products. Only the composition root enables it (RFC §2.1).

pub mod audit;
pub mod classification;
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

/// Object-safe async return type. Traits are used as `Arc<dyn Trait>`, so no
/// `async-trait` dependency and no native async-fn-in-trait (RFC §1 rule 2).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
