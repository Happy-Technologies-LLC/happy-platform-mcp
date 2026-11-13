//! rmcp `ServerHandler` shared by every transport: admission, then the dispatcher.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use axum::http::request::Parts;
use hpm_tools::dispatcher::Dispatcher;
use hpm_types::BoxFut;
use hpm_types::context::{
    AdmissionInput, DeploymentMode, ProgressSink, ProtocolVersion, TrustedRoot,
    VerifiedHttpIdentity,
};
use hpm_types::continuation::{
    ConnectPrompt, ContinuationBinding, ContinuationHandle, ContinuationStore, select_prompt,
};
use hpm_types::dispatch::DispatchOutcome;
use hpm_types::operation::Subject;
use hpm_types::registry::RegistryStore;
use hpm_types::tool::{ToolDescriptor, ToolError};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    DiscoverResult, Implementation, InitializeResult, JsonObject, ListToolsResult,
    PaginatedRequestParams, ProgressNotificationParam, ProgressToken, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::{Peer, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;

use crate::continuation::{
    self, CONTINUATION_TTL, MemoryContinuationStore, args_digest, redeem_error,
};

pub const SERVER_NAME: &str = "happy-platform-mcp";
/// Per-call deadline until the configuration lane supplies one.
const CALL_DEADLINE: Duration = Duration::from_secs(600);
/// C4 freshness hint for cacheable complete results on `2026-07-28`.
pub const LIST_TTL_MS: u64 = 300_000;

struct Inner {
    root: TrustedRoot,
    dispatcher: Arc<Dispatcher>,
    continuations: Arc<dyn ContinuationStore>,
    registry: Option<Arc<dyn RegistryStore>>,
}

/// Cheap to clone: Streamable HTTP builds one per session from a factory.
#[derive(Clone)]
pub struct McpHandler {
    inner: Arc<Inner>,
    /// Identity verified when a legacy SSE session was opened. The SSE adapter has
    /// no per-message `Parts`; every message on the session inherits it.
    session_identity: Option<VerifiedHttpIdentity>,
}

/// Builds an [`McpHandler`]. Optional services are set here, before the handler
/// exists, so a setting can never be dropped on an already shared handler.
pub struct McpHandlerBuilder {
    root: TrustedRoot,
    dispatcher: Arc<Dispatcher>,
    continuations: Option<Arc<dyn ContinuationStore>>,
    registry: Option<Arc<dyn RegistryStore>>,
}

impl McpHandlerBuilder {
    /// Replaces the single-process continuation store (enterprise: replicated).
    pub fn continuations(mut self, store: Arc<dyn ContinuationStore>) -> Self {
        self.continuations = Some(store);
        self
    }

    /// Resolves the call's instance from the registry view of the admitted
    /// principal (`args.instance`, else the selected default).
    pub fn registry(mut self, registry: Arc<dyn RegistryStore>) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn build(self) -> McpHandler {
        McpHandler {
            inner: Arc::new(Inner {
                root: self.root,
                dispatcher: self.dispatcher,
                continuations: self
                    .continuations
                    .unwrap_or_else(|| Arc::new(MemoryContinuationStore::default())),
                registry: self.registry,
            }),
            session_identity: None,
        }
    }
}

impl McpHandler {
    pub fn builder(root: TrustedRoot, dispatcher: Arc<Dispatcher>) -> McpHandlerBuilder {
        McpHandlerBuilder {
            root,
            dispatcher,
            continuations: None,
            registry: None,
        }
    }

    /// A handler with the in-memory continuation store and no registry.
    pub fn new(root: TrustedRoot, dispatcher: Arc<Dispatcher>) -> Self {
        Self::builder(root, dispatcher).build()
    }

    pub(crate) fn for_session(&self, identity: VerifiedHttpIdentity) -> Self {
        Self {
            inner: self.inner.clone(),
            session_identity: Some(identity),
        }
    }

    fn cache_scope(&self) -> CacheScope {
        match self.inner.root.mode() {
            DeploymentMode::SingleUser => CacheScope::Public,
            // Policy-filtered listings depend on the caller.
            _ => CacheScope::Private,
        }
    }
}

/// Request-bound progress: routed by the request's progress token; unroutable
/// progress is dropped, never broadcast (RFC §2.1 wiring step 3, C8).
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

fn http_identity(context: &RequestContext<RoleServer>) -> Option<VerifiedHttpIdentity> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|p| p.extensions.get::<VerifiedHttpIdentity>())
        .cloned()
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerConfig {
        // C9: tools only; no logging/prompts/sampling/roots.
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        // C4: discovery is static per listener.
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )
        .with_ttl_ms(LIST_TTL_MS)
        .with_cache_scope(self.cache_scope()))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut result = ListToolsResult::with_all_items(
            self.inner
                .dispatcher
                .tools()
                .iter()
                .map(to_rmcp_tool)
                .collect(),
        );
        // C4 on `2026-07-28` only: rmcp strips `resultType` for legacy peers but not
        // `ttlMs`/`cacheScope` (S1 G4), so legacy wires stay equal to 6.2.0.
        if context.meta.protocol_version().is_some() {
            result.ttl_ms = Some(LIST_TTL_MS);
            result.cache_scope = Some(self.cache_scope());
        }
        Ok(result)
    }

    /// rmcp's default `get_tool` returns `None` and bypasses lookup-based validation;
    /// it must delegate to the same registry as `call_tool`/`list_tools` (RFC §2.1).
    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.inner.dispatcher.get_tool(name).map(to_rmcp_tool)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let inner = &self.inner;
        // Modern requests carry version and capabilities per request (`_meta`);
        // legacy ones negotiated them at initialize.
        let peer_info = context.peer.peer_info();
        let protocol_version = context
            .meta
            .protocol_version()
            .or_else(|| peer_info.as_ref().map(|i| i.protocol_version.clone()))
            .map(|v| ProtocolVersion::new(v.as_str()))
            .unwrap_or_else(|| ProtocolVersion::new(""));
        let client_capabilities = context
            .meta
            .client_capabilities()
            .and_then(|c| serde_json::to_value(c).ok())
            .or_else(|| {
                peer_info
                    .as_ref()
                    .and_then(|i| serde_json::to_value(&i.capabilities).ok())
            })
            .unwrap_or(Value::Null);
        let client_name = context
            .meta
            .client_info()
            .map(|i| i.name)
            .or_else(|| peer_info.as_ref().map(|i| i.client_info.name.clone()))
            .unwrap_or_default();
        let identity = http_identity(&context).or_else(|| self.session_identity.clone());
        let args = Value::Object(request.arguments.unwrap_or_default());

        // Step 0: trusted admission. Stdio carries no HTTP identity.
        let mut input = AdmissionInput {
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
        if let Some(registry) = &inner.registry {
            // Principal is fixed by the transport: stdio is the local operator, HTTP
            // the verified identity. A missing HTTP identity fails in `admit`.
            let principal = identity
                .as_ref()
                .map(|i| i.principal.clone())
                .unwrap_or(hpm_types::context::Principal::LocalOperator);
            input.instance = registry
                .snapshot_for(&principal)
                .resolve(args.get("instance").and_then(Value::as_str));
        }
        let ctx = inner
            .root
            .admit(input, identity)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let subject_name = inner.dispatcher.get_tool(&request.name).map(|d| d.name);
        let binding = |expires_at: SystemTime| {
            subject_name.map(|name| ContinuationBinding {
                principal: ctx.principal().clone(),
                client_id: ctx.client().client_id.clone(),
                method: "tools/call",
                subject: Subject::Tool(name),
                args_digest: args_digest(&args),
                instance_id: ctx.instance().map(|i| i.instance_id.clone()),
                config_version: ctx.instance().map_or(0, |i| i.config_version),
                expires_at,
            })
        };

        // MRTR retry: redeem the single-use handle before anything is admitted.
        if let Some(state) = request.request_state.as_deref() {
            let presented = binding(SystemTime::now()).ok_or_else(|| {
                ErrorData::invalid_params(format!("Unknown tool: {}", request.name), None)
            })?;
            inner
                .continuations
                .redeem(&ContinuationHandle(state.to_owned()), &presented)
                .await
                .map_err(|e| redeem_error(&e))?;
        }

        match inner
            .dispatcher
            .call_tool(&ctx, &request.name, args.clone())
            .await
        {
            Ok(DispatchOutcome::Rendered(r)) => {
                let content = vec![ContentBlock::text(r.text)];
                Ok(if r.is_error {
                    CallToolResult::error(content)
                } else {
                    CallToolResult::success(content)
                }
                .into())
            }
            Ok(DispatchOutcome::ConnectRequired { connect_url }) => {
                // RFC §4.1: chosen from the verified version and capabilities.
                let handle = if ctx.protocol_version().supports_mrtr() {
                    let b = binding(SystemTime::now() + CONTINUATION_TTL)
                        .ok_or_else(|| ErrorData::internal_error("unknown subject", None))?;
                    inner
                        .continuations
                        .issue(b)
                        .await
                        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
                } else {
                    ContinuationHandle(String::new())
                };
                match select_prompt(
                    ctx.protocol_version(),
                    ctx.client_capabilities(),
                    connect_url,
                    handle,
                ) {
                    ConnectPrompt::InputRequired { url, handle } => {
                        Ok(continuation::input_required(&url, handle))
                    }
                    ConnectPrompt::LegacyUrlElicitation { url } => {
                        Err(continuation::legacy_url_elicitation(&url))
                    }
                    ConnectPrompt::ConnectLink { url } => Ok(continuation::connect_link(&url)),
                    _ => Err(ErrorData::internal_error("unexpected connect prompt", None)),
                }
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
