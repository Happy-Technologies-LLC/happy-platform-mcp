//! Tool families, one per W3 lane (plan p3 §2). Each exposes
//! `pub fn register(&mut ToolRegistry, &FamilyDeps) -> Result<(), RegistrationError>`;
//! the composition root calls [`register_all`]. Listing order is fixed by the
//! registry (D-05 row order = 6.2.0 `tools/list`), not by call order.

use std::path::PathBuf;
use std::sync::Arc;

use hpm_core::client::PinnedClient;
use hpm_core::config::ConfigManager;
use hpm_types::servicenow::ServiceNowAccess;

use crate::registration::{RegistrationError, ToolRegistry};

pub mod atf;
pub mod crud;
pub mod docs;
pub mod instances;
pub mod itsm;
pub mod update_sets;

/// What handlers may hold. Built once by the composition root.
#[derive(Clone)]
#[non_exhaustive]
pub struct FamilyDeps {
    /// The client pinned to the call's credential binding (G2). Handlers never
    /// pick an instance themselves.
    pub servicenow: Arc<dyn ServiceNowAccess<Client = PinnedClient>>,
    /// Instance registry and environment (6.2.0 `configManager`), for the
    /// instance tools (lane D).
    pub config: Arc<ConfigManager>,
    /// Raw docs base URL override (lane E); `None`: the 6.2.0 default.
    pub docs_base_url: Option<String>,
    /// Directory for files tools write (fix scripts, lane C); `None`: cwd.
    pub working_dir: Option<PathBuf>,
}

impl FamilyDeps {
    pub fn new(
        servicenow: Arc<dyn ServiceNowAccess<Client = PinnedClient>>,
        config: Arc<ConfigManager>,
    ) -> Self {
        Self {
            servicenow,
            config,
            docs_base_url: None,
            working_dir: None,
        }
    }
}

/// Every family, in 6.2.0 source order.
pub fn register_all(
    registry: &mut ToolRegistry,
    deps: &FamilyDeps,
) -> Result<(), RegistrationError> {
    crud::register(registry, deps)?;
    itsm::register(registry, deps)?;
    update_sets::register(registry, deps)?;
    atf::register(registry, deps)?;
    instances::register(registry, deps)?;
    docs::register(registry, deps)
}
