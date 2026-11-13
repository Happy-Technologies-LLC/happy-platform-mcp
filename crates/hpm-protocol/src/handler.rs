//! rmcp `ServerHandler` shared by every transport: admission, then the dispatcher.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hpm_tools::dispatcher::Dispatcher;
use hpm_types::BoxFut;
use hpm_types::context::{AdmissionInput, ProgressSink, ProtocolVersion, TrustedRoot};
use hpm_types::dispatch::DispatchOutcome;
use hpm_types::tool::{ToolDescriptor, ToolError};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ProgressToken, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{Peer, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};

pub const SERVER_NAME: &str = "happy-platform-mcp";
/// Per-call deadline until the configuration lane supplies one.
const CALL_DEADLINE: Duration = Duration::from_secs(600);

pub struct McpHandler {
    root: TrustedRoot,
    dispatcher: Arc<Dispatcher>,
}

impl McpHandler {
    pub fn new(root: TrustedRoot, dispatcher: Arc<Dispatcher>) -> Self {
        Self { root, dispatcher }
    }
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

fn to_rmcp_tool(d: &ToolDescriptor) -> Tool {
    // Registration guarantees the schema parses as an object.
    let schema: JsonObject = serde_json::from_str(d.input_schema_json).unwrap_or_default();
    Tool::new(d.name, d.description, Arc::new(schema))
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerConfig {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            self.dispatcher.tools().iter().map(to_rmcp_tool).collect(),
        ))
    }

    /// rmcp's default `get_tool` returns `None` and bypasses lookup-based validation;
    /// it must delegate to the same registry as `call_tool`/`list_tools` (RFC §2.1).
    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.dispatcher.get_tool(name).map(to_rmcp_tool)
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
        let ctx = self
            .root
            .admit(input, None)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
        match self.dispatcher.call_tool(&ctx, &request.name, args).await {
            Ok(DispatchOutcome::Rendered(r)) => {
                let content = vec![ContentBlock::text(r.text)];
                Ok(if r.is_error {
                    CallToolResult::error(content)
                } else {
                    CallToolResult::success(content)
                }
                .into())
            }
            // Lane E replaces this with the continuation + ConnectPrompt mapping
            // (RFC §4.1); until then the operator gets the sign-in URL as text.
            Ok(DispatchOutcome::ConnectRequired { connect_url }) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Sign-in required. Open {connect_url} and retry the call."
                ))])
                .into())
            }
            Ok(_) => Err(ErrorData::internal_error(
                "unexpected dispatch outcome",
                None,
            )),
            // C6: lookup and validation failures are JSON-RPC invalid params.
            Err(ToolError::InvalidParams(msg)) => Err(ErrorData::invalid_params(msg, None)),
            // Every other failure is a tool result with isError (6.2.0 parity).
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())]).into()),
        }
    }
}
