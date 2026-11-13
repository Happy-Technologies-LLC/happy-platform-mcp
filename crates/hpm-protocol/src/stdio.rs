//! Single-user stdio transport. The root must be `TrustedRoot::single_user_stdio()`.

use std::sync::Arc;

use hpm_tools::dispatcher::Dispatcher;
use hpm_types::context::TrustedRoot;
use rmcp::ServiceExt;

use crate::handler::McpHandler;

#[derive(Debug, thiserror::Error)]
pub enum StdioError {
    #[error("stdio initialization failed: {0}")]
    Init(String),
    #[error("stdio service failed: {0}")]
    Service(String),
}

/// Serves MCP over stdin/stdout until the client disconnects.
pub async fn serve(root: TrustedRoot, dispatcher: Arc<Dispatcher>) -> Result<(), StdioError> {
    let running = McpHandler::new(root, dispatcher)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| StdioError::Init(e.to_string()))?;
    running
        .waiting()
        .await
        .map(|_| ())
        .map_err(|e| StdioError::Service(e.to_string()))
}
