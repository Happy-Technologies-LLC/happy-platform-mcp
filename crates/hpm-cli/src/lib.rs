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

/// 6.2.0 `booleanEnv` (`src/stdio-server.js:31-33`).
fn boolean_env(value: Option<&str>) -> bool {
    matches!(
        value.unwrap_or("").to_lowercase().as_str(),
        "true" | "1" | "yes"
    )
}

/// 6.2.0 `hasServiceNowEnvCredentials` (`src/stdio-server.js:35-45`).
fn has_servicenow_env_credentials(env: &hpm_core::config::Env) -> bool {
    let set = |k: &str| env.get(k).is_some_and(|v| !v.is_empty());
    let grant = env.get("SERVICENOW_OAUTH_GRANT_TYPE");
    let passwordless = matches!(
        grant.as_deref(),
        Some("client_credentials" | "authorization_code")
    );
    set("SERVICENOW_INSTANCE_URL")
        && (passwordless || (set("SERVICENOW_USERNAME") && set("SERVICENOW_PASSWORD")))
}

/// The stdio server's mode: docs-only, or the resolved default instance's name and url.
enum StdioMode {
    DocsOnly,
    Instance { name: String, url: String },
}

fn field(instance: &hpm_core::config::Object, key: &str) -> String {
    match instance.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "undefined".to_string(),
    }
}

/// 6.2.0 `createConfiguredMcpServer` mode decision (`src/stdio-server.js:47-104`):
/// `HAPPY_MCP_DOCS_ONLY` true/1/yes (any case) forces docs-only; otherwise the
/// default instance is resolved and a "Missing ServiceNow credentials" /
/// "servicenow-instances.json not found" error falls back to docs-only unless
/// `HAPPY_MCP_DOCS_ONLY=false` (any case) or env credentials are set. Any other
/// resolution error aborts startup with exit 1 — including `REGISTRY_EMPTY` for a
/// registry file with zero instances, which 6.2.0 also refuses (parity, not docs-only).
/// `--docs-only` itself is not read (`src/cli.js:34` only routes it to `main()`), so
/// it serves docs-only only through this fallback. Prints the pre-connect stderr lines.
fn stdio_mode(config: &ConfigManager) -> Result<StdioMode, String> {
    let env = config.env();
    let flag = env.get("HAPPY_MCP_DOCS_ONLY");
    let docs_only = || {
        // `src/stdio-server.js:80`
        eprintln!("📚 Starting Happy MCP in docs-only mode");
        Ok(StdioMode::DocsOnly)
    };
    if boolean_env(flag.as_deref()) {
        return docs_only();
    }
    let instance = env.get("SERVICENOW_INSTANCE");
    match config.get_instance_or_default(instance.as_deref()) {
        Ok(instance) => {
            let (name, url) = (field(&instance, "name"), field(&instance, "url"));
            // `src/stdio-server.js:103-104`
            eprintln!("🔗 Default ServiceNow instance: {name} ({url})");
            eprintln!("💡 Use SN-Set-Instance tool to switch instances during session");
            Ok(StdioMode::Instance { name, url })
        }
        Err(e) => {
            let msg = e.message.to_lowercase();
            let explicitly_disabled = flag.as_deref().unwrap_or("").to_lowercase() == "false";
            if !explicitly_disabled
                && !has_servicenow_env_credentials(env)
                && (msg.contains("missing servicenow credentials")
                    || msg.contains("servicenow-instances.json not found"))
            {
                docs_only()
            } else {
                Err(format!("Failed to start Happy MCP Server: {}", e.message))
            }
        }
    }
}

/// Mints the process's one `AdmissionKey` and builds the shared dispatcher over the
/// OS keychain and the instance registry of `config`. Errors are ready to print
/// after the `NAME: ` prefix. `usage` counts the calls (6.2.0 `defaultUsageTracker()`).
fn single_user_dispatcher(
    config: Arc<ConfigManager>,
    usage: &UsageTracker,
) -> Result<Arc<Dispatcher>, String> {
    build_dispatcher(config, usage, false)
}

fn build_dispatcher(
    config: Arc<ConfigManager>,
    usage: &UsageTracker,
    docs_only: bool,
) -> Result<Arc<Dispatcher>, String> {
    let key = AdmissionKey::mint()
        .ok_or_else(|| "admission key already minted in this process".to_string())?;
    SingleUser::from_process(config)?
        .docs_only(docs_only)
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
    let mode = match stdio_mode(&config) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let docs_only = matches!(mode, StdioMode::DocsOnly);
    let dispatcher = match build_dispatcher(config.clone(), &usage, docs_only) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let handler = McpHandler::builder(TrustedRoot::single_user_stdio(), dispatcher)
        .registry(config)
        .build();
    // 6.2.0 `src/stdio-server.js:132-141`, after `server.connect`; stderr only
    // (stdout carries the protocol).
    eprintln!("Happy MCP Server (stdio) started successfully");
    match &mode {
        StdioMode::DocsOnly => eprintln!("Mode: docs-only"),
        StdioMode::Instance { name, url } => eprintln!("Instance: {name} - {url}"),
    }
    eprintln!("{}", updates_hint());
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
