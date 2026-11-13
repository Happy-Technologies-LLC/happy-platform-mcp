//! Composition root (plan §5.1). The only crate that enables `hpm-types/trusted-root`:
//! it mints the `TrustedRoot` for the protocol adapter and the `AdmissionKey` for the
//! dispatcher, chooses the service implementations and registers the tools.
//!
//! Baseline: single-user stdio, zero tools. Diagnostics go to stderr; stdout carries
//! only JSON-RPC (C10).

use std::process::ExitCode;
use std::sync::Arc;

use hpm_tools::dispatcher::{Dispatcher, Services};
use hpm_tools::registration::{RegistrationError, ToolRegistry};
use hpm_tools::schema::{CompiledSchemaValidator, SchemaCompileError};
use hpm_tools::single_user::{
    AllowAll, LocalOnlyCredentials, PassThrough, StderrAudit, UnloadedClassification,
};
use hpm_types::context::{AdmissionKey, TrustedRoot};
use hpm_types::operation::ScriptOperandGuard;

pub mod cli;
pub mod instance_cli;

const NAME: &str = "happy-platform-mcp";

/// Public page for release notes and advisories (`project-links.js`).
pub const UPDATES_URL: &str = "https://happy-tech.biz/mcp";

/// One-line pointer printed to stderr after a successful setup step.
pub fn updates_hint() -> String {
    format!("Release notes and security advisories: {UPDATES_URL}")
}

/// Every tool lane's `register`, in 6.2.0 `tools/list` order (C5).
fn register_tools(_registry: &mut ToolRegistry) -> Result<(), RegistrationError> {
    Ok(())
}

fn single_user_services(registry: &ToolRegistry) -> Result<Services, SchemaCompileError> {
    Ok(Services {
        validator: Arc::new(CompiledSchemaValidator::new(registry.tools())?),
        classification: Arc::new(UnloadedClassification),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: Arc::new(LocalOnlyCredentials),
        audit: Arc::new(StderrAudit),
        // Single-user: the operator's own ServiceNow account.
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
    })
}

/// Entry point of both binaries (`happy-platform-mcp`, `servicenow-mcp-server`):
/// `cli.js` dispatch over the process arguments.
pub fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    cli::main(&args)
}

/// Serve MCP over stdio (no arguments, or `--docs-only`).
pub fn serve_stdio() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{NAME}: runtime start failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let registry = ToolRegistry::new().and_then(|mut r| register_tools(&mut r).map(|()| r));
    let registry = match registry {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{NAME}: tool registration failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(key) = AdmissionKey::mint() else {
        eprintln!("{NAME}: admission key already minted in this process");
        return ExitCode::FAILURE;
    };
    let services = match single_user_services(&registry) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let dispatcher = Arc::new(Dispatcher::new(registry, services, key));
    match runtime.block_on(hpm_protocol::stdio::serve(
        TrustedRoot::single_user_stdio(),
        dispatcher,
    )) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            ExitCode::FAILURE
        }
    }
}
