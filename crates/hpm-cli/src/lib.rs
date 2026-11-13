//! Composition root (plan §5.1). The only crate that enables `hpm-types/trusted-root`:
//! it mints the `TrustedRoot` for the protocol adapter and the `AdmissionKey` for the
//! dispatcher, chooses the service implementations and registers the tools.
//!
//! Baseline: single-user stdio, zero tools. Diagnostics go to stderr; stdout carries
//! only JSON-RPC (C10).

use std::process::ExitCode;
use std::sync::Arc;

use hpm_tools::dispatcher::{Dispatcher, RequiredPropertiesValidator, Services};
use hpm_tools::registration::{RegistrationError, ToolRegistry};
use hpm_tools::single_user::{
    AllowAll, LocalOnlyCredentials, PassThrough, StderrAudit, UnloadedClassification,
};
use hpm_types::context::{AdmissionKey, TrustedRoot};
use hpm_types::operation::ScriptOperandGuard;

const NAME: &str = "happy-platform-mcp";

/// Every tool lane's `register`, in 6.2.0 `tools/list` order (C5).
fn register_tools(_registry: &mut ToolRegistry) -> Result<(), RegistrationError> {
    Ok(())
}

fn single_user_services() -> Services {
    Services {
        validator: Arc::new(RequiredPropertiesValidator),
        classification: Arc::new(UnloadedClassification),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: Arc::new(LocalOnlyCredentials),
        audit: Arc::new(StderrAudit),
        // Single-user: the operator's own ServiceNow account.
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
    }
}

/// Entry point of both binaries (`happy-platform-mcp`, `servicenow-mcp-server`).
pub fn run() -> ExitCode {
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
    let dispatcher = Arc::new(Dispatcher::new(registry, single_user_services(), key));
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
