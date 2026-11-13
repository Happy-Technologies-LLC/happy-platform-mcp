//! Composition root (plan §5.1). The only crate that enables `hpm-types/trusted-root`.
//!
//! Baseline: single-user stdio transport, trusted admission wired, zero tools. Tool
//! lanes register descriptors through the dispatcher; HTTP listeners and enterprise
//! wiring come later. stdout carries only JSON-RPC (C10); diagnostics go to stderr.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hpm_types::BoxFut;
use hpm_types::context::{AdmissionInput, ProgressSink, ProtocolVersion, TrustedRoot};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ProgressNotificationParam, ProgressToken, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::{Peer, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};

const SERVER_NAME: &str = "happy-platform-mcp";
/// Per-call deadline until the configuration lane supplies one.
const CALL_DEADLINE: Duration = Duration::from_secs(600);

struct Server {
    root: TrustedRoot,
}

/// Request-bound progress: routed by the request's progress token; unroutable
/// progress is dropped, never broadcast (RFC §2.1 wiring step 3).
struct PeerProgress {
    peer: Peer<RoleServer>,
    token: Option<ProgressToken>,
}

impl ProgressSink for PeerProgress {
    fn emit(&self, progress: f64, total: Option<f64>, message: Option<String>) -> BoxFut<'_, ()> {
        Box::pin(async move {
            let Some(token) = self.token.clone() else {
                return;
            };
            let mut param = ProgressNotificationParam::new(token, progress);
            param.total = total;
            param.message = message;
            // A closed peer cannot receive progress; the call itself reports the outcome.
            let _ = self.peer.notify_progress(param).await;
        })
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(Vec::new()))
    }

    /// rmcp's default `get_tool` returns `None` and bypasses lookup-based validation;
    /// it must delegate to the same registry as `call_tool`/`list_tools` (RFC §2.1).
    fn get_tool(&self, _name: &str) -> Option<Tool> {
        None
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Step 0: trusted admission. Stdio carries no HTTP identity.
        let peer_info = context.peer.peer_info();
        let protocol_version = context
            .meta
            .protocol_version()
            .or_else(|| peer_info.as_ref().map(|i| i.protocol_version.clone()))
            .map(|v| ProtocolVersion::new(v.as_str()))
            .unwrap_or_else(|| ProtocolVersion::new(""));
        let client_capabilities = peer_info
            .as_ref()
            .and_then(|i| serde_json::to_value(&i.capabilities).ok())
            .unwrap_or(serde_json::Value::Null);
        let client_name = peer_info
            .as_ref()
            .map(|i| i.client_info.name.clone())
            .unwrap_or_default();
        let input = AdmissionInput {
            mcp_request_id: context.id.to_string(),
            protocol_version,
            client_capabilities,
            client_name,
            instance: None,
            cancel: context.ct.child_token(),
            deadline: Instant::now() + CALL_DEADLINE,
            progress: Arc::new(PeerProgress {
                peer: context.peer.clone(),
                token: context.meta.get_progress_token(),
            }),
        };
        if let Err(e) = self.root.admit(input, None) {
            return Err(ErrorData::internal_error(e.to_string(), None));
        }
        // Step 1: lookup. No tools are registered yet, so every name is unknown (C6).
        Err(ErrorData::invalid_params(
            format!("Unknown tool: {}", request.name),
            None,
        ))
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let server = Server {
        root: TrustedRoot::single_user_stdio(),
    };
    let running = match server.serve(rmcp::transport::stdio()).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{SERVER_NAME}: stdio initialization failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    match running.waiting().await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{SERVER_NAME}: stdio service failed: {e}");
            ExitCode::FAILURE
        }
    }
}
