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
use hpm_tools::families::docs::telemetry::UsageTracker;
use hpm_tools::families::{self, FamilyDeps};
use hpm_tools::registration::{RegistrationError, ToolRegistry};
use hpm_tools::schema::{CompiledSchemaValidator, SchemaCompileError};
use hpm_tools::single_user::{AllowAll, PassThrough, StderrAudit, UnloadedClassification};
use hpm_types::context::{AdmissionKey, TrustedRoot};
use hpm_types::credential::CredentialProvider;
use hpm_types::dispatch::UsageSink;
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
    usage: Option<UsageTracker>,
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
        // 6.2.0 usage telemetry: opt-in, counts registered tool names only.
        usage: usage.map(|tracker| Arc::new(tracker) as Arc<dyn UsageSink>),
    })
}

/// An extra registration hook for [`SingleUser::extra_tools`].
pub type ExtraTools = fn(&mut ToolRegistry, &FamilyDeps) -> Result<(), RegistrationError>;

/// Explicit settings for the single-user composition, so tests and embedders can
/// point it at loopback stubs. [`SingleUser::from_process`] is what the binaries use.
pub struct SingleUser {
    config: Arc<ConfigManager>,
    client_deps: ClientDeps,
    docs_base_url: Option<String>,
    working_dir: Option<PathBuf>,
    docs_cache_dir: Option<PathBuf>,
    docs_only: bool,
    usage: Option<UsageTracker>,
    extra: Vec<ExtraTools>,
}

impl SingleUser {
    /// Process environment, default registry paths, OS keychain, reqwest.
    pub fn from_process(config: Arc<ConfigManager>) -> Result<Self, String> {
        let root = Self::new(
            config,
            ClientDeps::production(Arc::new(LazySecretStore::os()))
                .map_err(|e| format!("ServiceNow client setup failed: {e}"))?,
        );
        Ok(with_startup_dir(root, std::env::current_dir()))
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
            docs_cache_dir: None,
            docs_only: false,
            usage: None,
            extra: Vec::new(),
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

    /// Docs cache directory (lane E). Default: the 6.2.0 location.
    pub fn docs_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.docs_cache_dir = Some(dir.into());
        self
    }

    /// `--docs-only`: list and serve only the 6.2.0 `docsOnlyList` tools.
    pub fn docs_only(mut self, on: bool) -> Self {
        self.docs_only = on;
        self
    }

    /// Counts each `tools/call` in `tracker` (6.2.0 `usageTracker.record`). Default:
    /// no tracker, nothing counted. The caller keeps a clone to flush on shutdown.
    pub fn usage(mut self, tracker: UsageTracker) -> Self {
        self.usage = Some(tracker);
        self
    }

    /// Test-only extra registrations, run after every family (e.g. the golden
    /// harness's stub tools), so harnesses reuse these exact `Services`.
    pub fn extra_tools(mut self, register: ExtraTools) -> Self {
        self.extra.push(register);
        self
    }

    pub fn config(&self) -> &Arc<ConfigManager> {
        &self.config
    }

    /// Registers every family and builds the dispatcher with `key` (the process
    /// mints exactly one).
    pub fn dispatcher(self, key: AdmissionKey) -> Result<Arc<Dispatcher>, String> {
        let credential_store = self.client_deps.credential_store.clone();
        let clients = Arc::new(InstanceClients::from_config(
            self.config.clone(),
            self.client_deps,
        ));
        let mut family_deps = FamilyDeps::new(clients.clone(), self.config, credential_store);
        family_deps.docs_base_url = self.docs_base_url;
        family_deps.working_dir = self.working_dir;
        family_deps.docs_cache_dir = self.docs_cache_dir;
        family_deps.docs_only = self.docs_only;
        let mut registry = ToolRegistry::new()
            .and_then(|mut r| families::register_all(&mut r, &family_deps).map(|()| r))
            .and_then(|mut r| {
                for register in &self.extra {
                    register(&mut r, &family_deps)?;
                }
                Ok(r)
            })
            .map_err(|e| format!("tool registration failed: {e}"))?;
        if self.docs_only {
            registry.retain_docs_only();
        }
        // Read by resources at request time; set only after docs-only filtering.
        let _ = family_deps.tool_count.set(registry.tools().len());
        let credentials = Arc::new(SingleUserCredentialProvider::new(clients));
        let services =
            single_user_services(&registry, credentials, self.usage).map_err(|e| e.to_string())?;
        Ok(Arc::new(Dispatcher::new(registry, services, key)))
    }
}

/// Fix scripts land under the startup directory (review TF-11). If it cannot
/// be read, startup still succeeds and the directory is resolved when a fix
/// script is written (review L-5).
fn with_startup_dir(root: SingleUser, cwd: std::io::Result<PathBuf>) -> SingleUser {
    match cwd {
        Ok(cwd) => root.working_dir(cwd),
        Err(_) => root,
    }
}

/// Mints the process's one `AdmissionKey` and builds the shared dispatcher over the
/// OS keychain and the instance registry of `config`. Errors are ready to print
/// after the `NAME: ` prefix. `usage` counts the calls (6.2.0 `defaultUsageTracker()`).
fn single_user_dispatcher(
    config: Arc<ConfigManager>,
    usage: &UsageTracker,
) -> Result<Arc<Dispatcher>, String> {
    let key = AdmissionKey::mint()
        .ok_or_else(|| "admission key already minted in this process".to_string())?;
    SingleUser::from_process(config)?
        .usage(usage.clone())
        .dispatcher(key)
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
    let usage = UsageTracker::from_env(config.env());
    usage.set_transport("stdio");
    let dispatcher = match single_user_dispatcher(config.clone(), &usage) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let handler = McpHandler::builder(TrustedRoot::single_user_stdio(), dispatcher)
        .registry(config)
        .build();
    // 6.2.0 `stdio-server.js` prints the notice to stderr (stdout carries the protocol).
    if usage.enabled() {
        eprintln!("{}", usage.describe());
    }
    let served = runtime.block_on(async {
        let served = hpm_protocol::stdio::serve_handler(handler).await;
        usage.stop();
        usage.flush().await;
        served
    });
    // Never wait unboundedly for `spawn_blocking` work (e.g. a resolver or keychain call).
    runtime.shutdown_timeout(http::SHUTDOWN_GRACE);
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hpm_core::config::Env;
    use hpm_core::keychain::MemorySecretStore;
    use hpm_core::registry::InstanceRegistry;

    fn root() -> SingleUser {
        let env = Env::from_pairs(&[]);
        let registry = Arc::new(InstanceRegistry::new(env.clone()));
        let config = Arc::new(ConfigManager::new(registry, env));
        let deps = ClientDeps::production(Arc::new(MemorySecretStore::default())).expect("deps");
        SingleUser::new(config, deps)
    }

    /// L-5: an unreadable cwd leaves the directory to write time; TF-11: a
    /// readable one is pinned at startup.
    #[test]
    fn startup_dir_never_fails_startup() {
        let pinned = with_startup_dir(root(), Ok(PathBuf::from("/srv/app")));
        assert_eq!(pinned.working_dir, Some(PathBuf::from("/srv/app")));
        let lazy = with_startup_dir(root(), Err(std::io::Error::other("cwd deleted")));
        assert_eq!(lazy.working_dir, None);
    }
}
