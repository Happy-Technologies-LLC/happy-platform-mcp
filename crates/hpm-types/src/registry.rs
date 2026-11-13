//! RegistryStore: coherent per-principal views + conditional updates (F9, review edit 8).

use std::sync::Arc;

use crate::BoxFut;
use crate::context::{Principal, ResolvedInstance};

#[derive(Clone, Debug)]
pub struct InstanceEntry {
    /// Stable across rename; assigned once and persisted. Never a hash of `name`.
    pub instance_id: String,
    pub name: String,
    pub canonical_url: String,
    /// Non-secret auth metadata. Changing URL or client_id invalidates tokens (F6).
    pub auth: serde_json::Value,
    pub config_version: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DefaultSource {
    /// SU: `SERVICENOW_INSTANCE` env.
    Env,
    /// MU: the principal's own `SN-Set-Instance` selection.
    Principal,
    /// Organisation / file default.
    Registry,
    None,
}

/// Registry version AND the caller's selected default, captured together.
#[derive(Debug)]
pub struct RegistryView {
    pub registry_version: u64,
    pub instances: Vec<InstanceEntry>,
    pub selected_default: Option<String>,
    pub default_source: DefaultSource,
}

impl RegistryView {
    /// Resolution uses this view once per call.
    pub fn resolve(&self, requested: Option<&str>) -> Option<ResolvedInstance> {
        let id = requested.or(self.selected_default.as_deref())?;
        self.instances
            .iter()
            .find(|e| e.instance_id == id || e.name == id)
            .map(|e| ResolvedInstance {
                instance_id: e.instance_id.clone(),
                canonical_url: e.canonical_url.clone(),
                config_version: e.config_version,
            })
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum RegistryChange {
    Upsert(InstanceEntry),
    Remove {
        instance_id: String,
    },
    /// Organisation default; separately authorized (admin).
    SetRegistryDefault {
        instance_id: String,
    },
    /// One principal's selection; command service authorizes the actor for THIS principal.
    SetPrincipalDefault {
        principal: Principal,
        instance_id: String,
    },
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    #[error("version conflict: expected {expected}, current {current}")]
    Conflict { expected: u64, current: u64 },
    #[error("registry is read-only")]
    ReadOnly,
    #[error("invalid entry: {0}")]
    Invalid(String),
    #[error("registry storage")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait RegistryStore: Send + Sync {
    /// Immutable cached view; callers hold the Arc for the whole request. (An async
    /// variant is acceptable if MU needs a DB read; shape unchanged.)
    fn snapshot_for(&self, principal: &Principal) -> Arc<RegistryView>;
    /// Compare-and-swap on `expected_version`. Validation + authz happen in the
    /// config command service first (one path for hub, CLI, MCP tool).
    fn apply<'a>(
        &'a self,
        expected_version: u64,
        change: RegistryChange,
    ) -> BoxFut<'a, Result<u64, RegistryError>>;
}
