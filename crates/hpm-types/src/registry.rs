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
    /// SU env fallback (no registry file): a miss lists the available instances
    /// (6.2.0 `getInstance`, `config-manager.js:215`).
    pub env_fallback: bool,
}

/// 6.2.0 `resolveClient(args?.instance)`: a falsy value (absent, `null`, `""`,
/// `false`, `0`) means the default instance; anything else is looked up by its
/// JS string form.
pub fn requested_instance(arg: Option<&serde_json::Value>) -> Option<String> {
    use serde_json::Value;
    match arg? {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) if s.is_empty() => None,
        Value::Number(n) if n.as_f64() == Some(0.0) => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(true) => Some("true".into()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(a) => Some(
            a.iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(","),
        ),
        Value::Object(_) => Some("[object Object]".into()),
    }
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

    /// Resolves the per-call `instance` argument ([`requested_instance`]).
    /// `Err` carries the 6.2.0 not-found text for a named instance this view
    /// does not know; it never falls back to the default.
    pub fn resolve_requested(
        &self,
        arg: Option<&serde_json::Value>,
    ) -> Result<Option<ResolvedInstance>, String> {
        let Some(name) = requested_instance(arg) else {
            return Ok(self.resolve(None));
        };
        self.resolve(Some(&name))
            .map(Some)
            .ok_or_else(|| self.not_found(&name))
    }

    fn not_found(&self, name: &str) -> String {
        if self.env_fallback {
            let names: Vec<&str> = self.instances.iter().map(|e| e.name.as_str()).collect();
            format!(
                "Instance '{name}' not found. Available instances: {}",
                names.join(", ")
            )
        } else {
            format!("Instance '{name}' not found")
        }
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn view(env_fallback: bool) -> RegistryView {
        RegistryView {
            registry_version: 1,
            instances: vec![InstanceEntry {
                instance_id: "dev".into(),
                name: "dev".into(),
                canonical_url: "https://dev.example.com".into(),
                auth: json!({}),
                config_version: 1,
            }],
            selected_default: Some("dev".into()),
            default_source: DefaultSource::Registry,
            env_fallback,
        }
    }

    #[test]
    fn falsy_instance_means_default() {
        let v = view(false);
        for arg in [
            None,
            Some(json!(null)),
            Some(json!("")),
            Some(json!(false)),
            Some(json!(0)),
        ] {
            let got = v.resolve_requested(arg.as_ref()).expect("default");
            assert_eq!(got.map(|i| i.instance_id), Some("dev".into()), "{arg:?}");
        }
    }

    #[test]
    fn unknown_or_non_string_instance_is_not_found() {
        let v = view(false);
        assert_eq!(
            v.resolve_requested(Some(&json!("nope"))).err(),
            Some("Instance 'nope' not found".into())
        );
        assert_eq!(
            v.resolve_requested(Some(&json!(5))).err(),
            Some("Instance '5' not found".into())
        );
        assert_eq!(
            v.resolve_requested(Some(&json!({}))).err(),
            Some("Instance '[object Object]' not found".into())
        );
        assert_eq!(
            view(true).resolve_requested(Some(&json!("nope"))).err(),
            Some("Instance 'nope' not found. Available instances: dev".into())
        );
    }
}
