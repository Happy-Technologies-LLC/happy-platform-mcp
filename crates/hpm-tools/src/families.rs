//! Tool families, one per W3 lane (plan p3 §2). Each exposes
//! `pub fn register(&mut ToolRegistry, &FamilyDeps) -> Result<(), RegistrationError>`;
//! the composition root calls [`register_all`]. Listing order is fixed by the
//! registry (6.2.0 source order, [`crate::tool_order`]), not by call order.
//!
//! Paths: families take every path (registry file, working dir, docs cache) only
//! from [`FamilyDeps`] or its [`ConfigManager`], never from `std::env` or
//! `std::env::current_dir`, so tests and harnesses stay hermetic.

use std::path::PathBuf;
use std::sync::Arc;

use hpm_core::client::PinnedClient;
use hpm_core::config::ConfigManager;
use hpm_core::credential::InstanceCredentialStore;
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
    /// The credential store the ServiceNow clients read (6.2.0
    /// `dependencies.credentialStore`, `SN-Register-Instance`). Tests pass an
    /// in-memory store; never build the OS store inside a family.
    pub credentials: InstanceCredentialStore,
    /// Raw docs base URL override (lane E); `None`: the 6.2.0 default.
    pub docs_base_url: Option<String>,
    /// Directory for files tools write (fix scripts, lane C); `None`: cwd.
    pub working_dir: Option<PathBuf>,
    /// Docs cache directory override (lane E); `None`: the 6.2.0 default.
    pub docs_cache_dir: Option<PathBuf>,
}

impl FamilyDeps {
    pub fn new(
        servicenow: Arc<dyn ServiceNowAccess<Client = PinnedClient>>,
        config: Arc<ConfigManager>,
        credentials: InstanceCredentialStore,
    ) -> Self {
        Self {
            servicenow,
            config,
            credentials,
            docs_base_url: None,
            working_dir: None,
            docs_cache_dir: None,
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
