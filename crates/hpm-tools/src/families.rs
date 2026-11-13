//! Tool families, one per W3 lane (plan p3 §2). Each exposes
//! `pub fn register(&mut ToolRegistry, &FamilyDeps) -> Result<(), RegistrationError>`;
//! the composition root calls [`register_all`]. Listing order is fixed by the
//! registry (6.2.0 source order, [`crate::tool_order`]), not by call order.
//!
//! Paths: families take every path (registry file, working dir, docs cache) only
//! from [`FamilyDeps`], never from `std::env` or `std::env::current_dir`, so
//! tests and harnesses stay hermetic.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use hpm_core::client::PinnedClient;
use hpm_core::config::{ConfigManager, Env};
use hpm_core::credential::InstanceCredentialStore;
use hpm_types::servicenow::ServiceNowAccess;

use crate::registration::{RegistrationError, ToolRegistry};

pub mod atf;
pub mod common;
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
    /// Registry access for the instance tools only (CO-6): opaque outside
    /// [`instances`], so no other family can read or write the registry.
    instances: instances::InstancesAccess,
    /// The environment (6.2.0 `process.env`), read by docs settings and the
    /// `servicenow://instance` resource.
    pub env: Env,
    /// The instances file the docs settings (`docs` object) are read from.
    pub registry_read_path: PathBuf,
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
    /// `--docs-only` mode (6.2.0 `DOCS_ONLY`); e.g. SN-Register-Instance
    /// reports `restartRequired: true` in it.
    pub docs_only: bool,
    /// Final number of registered tools, set once by the composition root after
    /// every family registered and docs-only filtering ran. Read it at request
    /// time (`servicenow://instance` `capabilities.tools`), never at registration.
    pub tool_count: Arc<OnceLock<usize>>,
}

impl FamilyDeps {
    pub fn new(
        servicenow: Arc<dyn ServiceNowAccess<Client = PinnedClient>>,
        config: Arc<ConfigManager>,
        credentials: InstanceCredentialStore,
    ) -> Self {
        Self {
            servicenow,
            env: config.env().clone(),
            registry_read_path: config.registry().read_path(),
            instances: instances::InstancesAccess::new(config),
            credentials,
            docs_base_url: None,
            working_dir: None,
            docs_cache_dir: None,
            docs_only: false,
            tool_count: Arc::new(OnceLock::new()),
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
