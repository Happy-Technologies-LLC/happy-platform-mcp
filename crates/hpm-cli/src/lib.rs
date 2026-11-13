//! Composition root (plan §5.1). The only crate that enables `hpm-types/trusted-root`:
//! it mints the `TrustedRoot` for the protocol adapter and the `AdmissionKey` for the
//! dispatcher, chooses the service implementations and registers the tools.
//!
//! Single-user stdio (`src/stdio-server.js`) and HTTP (`src/server.js`). Tools come
//! from `hpm_tools::families`; ServiceNow access goes through one client per
//! registry instance (`InstanceClients`). In stdio mode diagnostics go to stderr;
//! stdout carries only JSON-RPC (C10).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use hpm_core::client::ClientDeps;
use hpm_core::config::ConfigManager;
use hpm_core::credential::provider::{InstanceClients, SingleUserCredentialProvider};
use hpm_core::keychain::LazySecretStore;
use hpm_protocol::McpHandler;
use hpm_tools::dispatcher::{Dispatcher, Services};
use hpm_tools::families::{self, FamilyDeps};
use hpm_tools::registration::ToolRegistry;
use hpm_tools::schema::{CompiledSchemaValidator, SchemaCompileError};
use hpm_tools::single_user::{AllowAll, PassThrough, StderrAudit, UnloadedClassification};
use hpm_types::context::{AdmissionKey, TrustedRoot};
use hpm_types::credential::CredentialProvider;
use hpm_types::operation::ScriptOperandGuard;

pub mod cli;
pub mod http;
pub mod instance_cli;

const NAME: &str = "happy-platform-mcp";

/// Public page for release notes and advisories (`project-links.js`).
pub const UPDATES_URL: &str = "https://happy-tech.biz/mcp";

/// One-line pointer printed to stderr after a successful setup step.
pub fn updates_hint() -> String {
    format!("Release notes and security advisories: {UPDATES_URL}")
}

fn single_user_services(
    registry: &ToolRegistry,
    credentials: Arc<dyn CredentialProvider>,
) -> Result<Services, SchemaCompileError> {
    Ok(Services {
        validator: Arc::new(CompiledSchemaValidator::new(registry.tools())?),
        classification: Arc::new(UnloadedClassification),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials,
        audit: Arc::new(StderrAudit),
        // Single-user: the operator's own ServiceNow account.
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
        // Lane E wires the 6.2.0 usage telemetry.
        usage: None,
    })
}

/// Explicit settings for the single-user composition, so tests and embedders can
/// point it at loopback stubs. [`SingleUser::from_process`] is what the binaries use.
pub struct SingleUser {
    config: Arc<ConfigManager>,
    client_deps: ClientDeps,
    docs_base_url: Option<String>,
    working_dir: Option<PathBuf>,
}

impl SingleUser {
    /// Process environment, default registry paths, OS keychain, reqwest.
    pub fn from_process(config: Arc<ConfigManager>) -> Result<Self, String> {
        Ok(Self::new(
            config,
            ClientDeps::production(Arc::new(LazySecretStore::os()))
                .map_err(|e| format!("ServiceNow client setup failed: {e}"))?,
        ))
    }

    /// `config` fixes the instances file and env map
    /// (`ConfigManager::new(Arc::new(InstanceRegistry::at_paths(..)), Env::from_pairs(..))`);
    /// `client_deps` the transport, clock, secret store and sign-in seams.
    pub fn new(config: Arc<ConfigManager>, client_deps: ClientDeps) -> Self {
        Self {
            config,
            client_deps,
            docs_base_url: None,
            working_dir: None,
        }
    }

    /// Raw docs base URL (6.2.0 docs tools; lane E). Default: the public one.
    pub fn docs_base_url(mut self, url: impl Into<String>) -> Self {
        self.docs_base_url = Some(url.into());
        self
    }

    /// Working directory for files the tools write (fix scripts). Default: cwd.
    pub fn working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn config(&self) -> &Arc<ConfigManager> {
        &self.config
    }

    /// Registers every family and builds the dispatcher with `key` (the process
    /// mints exactly one).
    pub fn dispatcher(self, key: AdmissionKey) -> Result<Arc<Dispatcher>, String> {
        let clients = Arc::new(InstanceClients::from_config(
            self.config.clone(),
            self.client_deps,
        ));
        let mut family_deps = FamilyDeps::new(clients.clone(), self.config);
        family_deps.docs_base_url = self.docs_base_url;
        family_deps.working_dir = self.working_dir;
        let registry = ToolRegistry::new()
            .and_then(|mut r| families::register_all(&mut r, &family_deps).map(|()| r))
            .map_err(|e| format!("tool registration failed: {e}"))?;
        let credentials = Arc::new(SingleUserCredentialProvider::new(clients));
        let services = single_user_services(&registry, credentials).map_err(|e| e.to_string())?;
        Ok(Arc::new(Dispatcher::new(registry, services, key)))
    }
}

/// Mints the process's one `AdmissionKey` and builds the shared dispatcher over the
/// OS keychain and the instance registry of `config`. Errors are ready to print
/// after the `NAME: ` prefix.
fn single_user_dispatcher(config: Arc<ConfigManager>) -> Result<Arc<Dispatcher>, String> {
    let key = AdmissionKey::mint()
        .ok_or_else(|| "admission key already minted in this process".to_string())?;
    SingleUser::from_process(config)?.dispatcher(key)
}

fn multi_thread_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime start failed: {e}"))
}

/// Entry point of both binaries (`happy-platform-mcp`, `servicenow-mcp-server`):
/// `cli.js` dispatch over the process arguments.
pub fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    cli::main(&args)
}

/// Serve MCP over stdio (no arguments, or `--docs-only`).
pub fn serve_stdio() -> ExitCode {
    let runtime = match multi_thread_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = Arc::new(ConfigManager::from_process_env());
    let dispatcher = match single_user_dispatcher(config.clone()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let handler = McpHandler::builder(TrustedRoot::single_user_stdio(), dispatcher)
        .registry(config)
        .build();
    match runtime.block_on(hpm_protocol::stdio::serve_handler(handler)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            ExitCode::FAILURE
        }
    }
}
