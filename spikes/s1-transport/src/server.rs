//! MCP handler shared by every transport: 60-tool `tools/list` from the 6.2.0
//! fixture, `SN-Query-Table` end to end, everything else answers "not in spike".

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock as Content,
        Implementation, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};
use serde::Deserialize;

use crate::sn;

pub const FIXTURE: &str = include_str!("../fixtures/tools-list-6.2.0.json");

#[derive(Deserialize)]
struct Fixture {
    tools: Vec<Tool>,
}

/// Fixture order (sorted by name) is deterministic (C5).
pub static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| {
    serde_json::from_str::<Fixture>(FIXTURE).expect("fixture parses").tools
});

static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

/// stderr-only event log (C10: stdout carries only JSON-RPC).
pub fn event(msg: impl AsRef<str>) {
    eprintln!("[hpm-s1 +{:>6}ms] {}", STARTED.elapsed().as_millis(), msg.as_ref());
}

#[derive(Clone)]
pub struct HpmServer {
    client: Arc<Result<sn::Client, String>>,
}

impl HpmServer {
    pub fn new(client: Arc<Result<sn::Client, String>>) -> Self {
        Self { client }
    }

    pub fn instance_label(&self) -> (String, String) {
        match self.client.as_ref() {
            Ok(c) => (c.instance().url.clone(), c.instance().name.clone()),
            Err(_) => (String::new(), String::new()),
        }
    }

    fn text_error(msg: impl std::fmt::Display) -> CallToolResult {
        // 6.2.0 envelope: `src/mcp-server-consolidated.js:3338-3346`.
        CallToolResult::error(vec![Content::text(format!("Error: {msg}"))])
    }

    async fn query_table(&self, args: serde_json::Map<String, serde_json::Value>) -> CallToolResult {
        let client = match self.client.as_ref() {
            Ok(c) => c,
            Err(e) => return Self::text_error(e),
        };
        let args: sn::QueryTableArgs = match serde_json::from_value(args.into()) {
            Ok(a) => a,
            Err(e) => return Self::text_error(e),
        };
        match client.get_records(&args).await {
            // `src/mcp-server-consolidated.js:1645-1651`.
            Ok(rows) => {
                let mut result = CallToolResult::success(vec![Content::text(format!(
                    "Found {} records in {}:\n{}",
                    rows.len(),
                    args.table_name,
                    serde_json::to_string_pretty(&rows).expect("serializable")
                ))]);
                // 6.2.0 omits `isError` on success.
                result.is_error = None;
                result
            }
            Err(e) => Self::text_error(e),
        }
    }
}

impl ServerHandler for HpmServer {
    fn get_info(&self) -> ServerConfig {
        // C9: tools only; no logging/prompts/sampling/roots.
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("happy-platform-mcp", "6.2.0"))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut result = ListToolsResult::with_all_items(TOOLS.clone());
        // C4 on `2026-07-28` only. rmcp clears `resultType` for legacy peers but not
        // `ttlMs`/`cacheScope`, so gate here to keep the legacy wire equal to 6.2.0.
        // Static in single-user, so `public`; multi-user (policy-filtered) must use `private`.
        if context.meta.protocol_version().is_some() {
            result.ttl_ms = Some(300_000);
            result.cache_scope = Some(CacheScope::Public);
        }
        Ok(result)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        TOOLS.iter().find(|t| t.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        if !TOOLS.iter().any(|t| t.name == name) {
            // C6: unknown tool is a protocol error.
            return Err(ErrorData::invalid_params(format!("Unknown tool: {name}"), None));
        }
        let id = format!("{:?}", context.id);
        event(format!("call start id={id} tool={name}"));
        // Observation hook for C7: optional delay before the outbound call.
        let delay = std::env::var("HPM_S1_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0u64);
        let work = async {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            match name.as_str() {
                "SN-Query-Table" => self.query_table(request.arguments.unwrap_or_default()).await,
                _ => Self::text_error(format!("{name} is not implemented in the S1 spike")),
            }
        };
        let probe = DropProbe(format!("handler future id={id} dropped before completion"));
        tokio::select! {
            result = work => {
                std::mem::forget(probe);
                event(format!("call done id={id} cancelled_token={}", context.ct.is_cancelled()));
                Ok(result.into())
            }
            _ = context.ct.cancelled() => {
                std::mem::forget(probe);
                event(format!("call cancelled id={id} (RequestContext.ct fired)"));
                Err(ErrorData::internal_error("cancelled", None))
            }
        }
    }
}

/// Drop guard that records when rmcp drops a handler future without firing `ct`.
pub struct DropProbe(pub String);
impl Drop for DropProbe {
    fn drop(&mut self) {
        event(format!("drop {}", self.0));
    }
}
