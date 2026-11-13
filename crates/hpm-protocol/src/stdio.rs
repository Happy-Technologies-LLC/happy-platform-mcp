//! Single-user stdio transport. The root must be `TrustedRoot::single_user_stdio()`.
//! C10: stdout carries only JSON-RPC; the service ends cleanly on stdin EOF.

use rmcp::ServiceExt;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::handler::McpHandler;

#[derive(Debug, thiserror::Error)]
pub enum StdioError {
    #[error("stdio initialization failed: {0}")]
    Init(String),
    #[error("stdio service failed: {0}")]
    Service(String),
}

/// Serves MCP over stdin/stdout until the client disconnects. Build the handler
/// with its registry (`McpHandler::builder(..).registry(..)`), or every
/// instance-bound tool fails with no instance.
pub async fn serve_handler(handler: McpHandler) -> Result<(), StdioError> {
    serve_io(handler, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Same as [`serve_handler`] over any byte stream pair (tests, embedding).
pub async fn serve_io<R, W>(handler: McpHandler, read: R, write: W) -> Result<(), StdioError>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let running = handler
        .serve((read, write))
        .await
        .map_err(|e| StdioError::Init(e.to_string()))?;
    running
        .waiting()
        .await
        .map(|_| ())
        .map_err(|e| StdioError::Service(e.to_string()))
}
