//! Single-user HTTP listener: one axum app serving `/mcp` per plan §5.3 rules 1–8.
//!
//! - rmcp `StreamableHttpService` (legacy session mode) serves `2026-07-28`
//!   stateless requests and `2025-*` sessions.
//! - The legacy HTTP+SSE adapter ([`super::sse`]) shares `/mcp`.
//! - The front guard (rate limit → Host → Origin → bearer) runs before any body
//!   read or session allocation and inserts the verified identity.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::serve::IncomingStream;
use axum::{Json, Router};
use hpm_types::context::VerifiedHttpIdentity;
use rmcp::model::ProtocolVersion;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::auth::{AuthFailureLimiter, operator_identity};
use super::json_error;
use super::limits::{HttpLimits, HttpSecurity, host_allowed, origin_allowed};
use super::sse::{DEFAULT_KEEPALIVE, SseSessions};
use crate::handler::McpHandler;

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const META_VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const MODERN: &str = "2026-07-28";

/// Body builders for the 6.2.0 `/health` and `/instances` routes.
pub type JsonFn = Arc<dyn Fn() -> Value + Send + Sync>;

pub struct HttpConfig {
    pub security: HttpSecurity,
    pub limits: HttpLimits,
    pub keepalive: Duration,
    /// `/health` body (`status`, `servicenow_instance`, `instance_name`, `timestamp`).
    pub health: JsonFn,
    /// `/instances` list; wrapped as `{"instances": …}`.
    pub instances: JsonFn,
}

impl HttpConfig {
    pub fn new(security: HttpSecurity, health: JsonFn, instances: JsonFn) -> Self {
        Self {
            security,
            limits: HttpLimits::default(),
            keepalive: DEFAULT_KEEPALIVE,
            health,
            instances,
        }
    }
}

/// Local and remote socket addresses of the accepted connection (Host check and
/// per-peer failure limiting).
#[derive(Clone, Copy, Debug)]
pub struct PeerAddrs {
    pub local: SocketAddr,
    pub remote: SocketAddr,
}

impl Connected<IncomingStream<'_, TcpListener>> for PeerAddrs {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        let remote = *stream.remote_addr();
        Self {
            local: stream.io().local_addr().unwrap_or(remote),
            remote,
        }
    }
}

struct AppState {
    security: HttpSecurity,
    max_json_bytes: usize,
    auth_failures: AuthFailureLimiter,
    rmcp: StreamableHttpService<McpHandler, LocalSessionManager>,
    sse: Arc<SseSessions>,
    health: JsonFn,
    instances: JsonFn,
}

/// The HTTP application. `streamable` must come from
/// `TrustedRoot::streamable_http`, `sse` from `TrustedRoot::legacy_sse`. Clones
/// share one listener state (keep one to call [`HttpApp::close_all_sessions`]).
#[derive(Clone)]
pub struct HttpApp {
    state: Arc<AppState>,
    shutdown: CancellationToken,
}

impl HttpApp {
    pub fn new(streamable: McpHandler, sse: McpHandler, config: HttpConfig) -> Self {
        let HttpConfig {
            security,
            limits,
            keepalive,
            health,
            instances,
        } = config;
        let shutdown = CancellationToken::new();
        // Host checks are ours (rule 1); rmcp re-checks Origin as defence in depth.
        let rmcp_config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(true)
            .disable_allowed_hosts()
            .with_allowed_origins(security.allowed_origins.iter().cloned().collect::<Vec<_>>())
            .enforce_origin_validation()
            .with_max_request_body_bytes(limits.max_json_bytes)
            .with_sse_keep_alive(Some(keepalive))
            .with_cancellation_token(shutdown.child_token());
        let rmcp = StreamableHttpService::new(
            move || Ok(streamable.clone()),
            Arc::new(LocalSessionManager::default()),
            rmcp_config,
        );
        let state = Arc::new(AppState {
            auth_failures: AuthFailureLimiter::new(
                limits.auth_failure_limit,
                limits.auth_failure_window,
                limits.max_tracked_auth_peers,
            ),
            max_json_bytes: limits.max_json_bytes,
            sse: SseSessions::new(sse, limits, keepalive),
            security,
            rmcp,
            health,
            instances,
        });
        Self { state, shutdown }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/mcp", any(mcp))
            .route("/health", get(health))
            .route("/instances", get(instances))
            .layer(middleware::from_fn_with_state(self.state.clone(), guard))
            .with_state(self.state.clone())
    }

    /// Refuses new SSE sessions and closes live ones and rmcp sessions with 503 semantics.
    pub fn close_all_sessions(&self) {
        self.state.sse.close_all();
        self.shutdown.cancel();
    }

    pub fn sse_session_count(&self) -> usize {
        self.state.sse.session_count()
    }

    /// Serves until `shutdown` resolves, then closes every session.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let router = self.router();
        let service = router.into_make_service_with_connect_info::<PeerAddrs>();
        let token = self.shutdown.clone();
        let sse = self.state.sse.clone();
        axum::serve(listener, service)
            .with_graceful_shutdown(async move {
                shutdown.await;
                sse.close_all();
                token.cancel();
            })
            .await
    }
}

fn jsonrpc_error(id: Value, code: i64, msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })),
    )
        .into_response()
}

fn too_many_failures(retry_after: u64) -> Response {
    let mut r = json_error(
        StatusCode::TOO_MANY_REQUESTS,
        "Too many failed authentication attempts",
    );
    if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
        r.headers_mut().insert(header::RETRY_AFTER, v);
    }
    r
}

fn peer_key(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or_else(|| v6.to_string(), |v4| v4.to_string()),
        v4 => v4.to_string(),
    }
}

async fn guard(State(st): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let Some(ConnectInfo(addrs)) = req.extensions().get::<ConnectInfo<PeerAddrs>>().copied() else {
        // Served without connect info: no Host or peer basis, fail closed.
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
    };
    let peer = peer_key(addrs.remote.ip());
    let now = Instant::now();
    if let Some(retry) = st.auth_failures.retry_after_seconds(&peer, now) {
        return too_many_failures(retry);
    }
    let h = req.headers();
    if !host_allowed(h, addrs.local, &st.security.allowed_hosts) {
        return json_error(StatusCode::FORBIDDEN, "Host not allowed");
    }
    if !origin_allowed(h, &st.security.allowed_origins) {
        return json_error(StatusCode::FORBIDDEN, "Origin not allowed");
    }
    if !st.security.token.matches(h) {
        if !st.auth_failures.record_failure(&peer, now) {
            let window = st.auth_failures.window().as_millis().div_ceil(1000) as u64;
            return too_many_failures(window);
        }
        let mut r = json_error(StatusCode::UNAUTHORIZED, "Unauthorized");
        r.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return r;
    }
    req.extensions_mut()
        .insert(operator_identity(&st.security.token));
    next.run(req).await
}

fn query_session_id(req: &Request) -> Option<String> {
    req.uri().query()?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "sessionId").then(|| v.to_string())
    })
}

fn accepts_sse(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|m| {
            m.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        })
}

fn has_modern_meta(body: &Value) -> bool {
    body.get("params")
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get(META_VERSION_KEY))
        .and_then(Value::as_str)
        .is_some_and(|v| v >= MODERN)
}

fn is_supported_version(v: &str) -> bool {
    ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .any(|k| k.as_str() == v)
}

async fn forward(st: &AppState, parts: axum::http::request::Parts, body: Body) -> Response {
    let resp = st.rmcp.handle(Request::from_parts(parts, body)).await;
    let (parts, body) = resp.into_parts();
    let plain_error = parts.status.is_client_error()
        && parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/plain"));
    if !plain_error {
        return Response::from_parts(parts, Body::new(body));
    }
    // Owner S1 decision: rmcp plain-text transport errors are wrapped as JSON.
    let text = axum::body::to_bytes(Body::new(body), 64 * 1024)
        .await
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .unwrap_or_default();
    let mut r = json_error(parts.status, &text);
    for name in [header::ALLOW, header::RETRY_AFTER] {
        if let Some(v) = parts.headers.get(&name) {
            r.headers_mut().insert(name, v.clone());
        }
    }
    r
}

async fn mcp(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let query_sid = query_session_id(&req);
    let header_sid = req.headers().contains_key(SESSION_HEADER);
    // Rule 2.
    if query_sid.is_some() && header_sid {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Both sessionId query and Mcp-Session-Id header supplied",
        );
    }
    let (parts, body) = req.into_parts();
    match parts.method {
        Method::POST => {
            let Ok(bytes) = axum::body::to_bytes(body, st.max_json_bytes).await else {
                return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
            };
            let parsed = serde_json::from_slice::<Value>(&bytes);
            // Rule 7: legacy HTTP+SSE POST keeps 6.2.0 body errors.
            if let Some(sid) = &query_sid
                && !parsed.as_ref().is_ok_and(has_modern_meta)
            {
                return st.sse.post(sid, &parts.headers, bytes).await;
            }
            // Rule 8: malformed JSON-RPC → 400 / -32700 before any routing.
            let Ok(value) = parsed else {
                return jsonrpc_error(Value::Null, -32700, "Parse error");
            };
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            if let Some(v) = parts.headers.get(VERSION_HEADER)
                && !v.to_str().is_ok_and(is_supported_version)
            {
                return jsonrpc_error(id, -32022, "Unsupported protocol version");
            }
            // Rule 3: modern `_meta` → stateless; a stale session id never downgrades.
            let mut parts = parts;
            if has_modern_meta(&value) {
                parts.headers.remove(SESSION_HEADER);
            }
            forward(&st, parts, Body::from(bytes)).await
        }
        Method::GET => {
            if header_sid {
                return forward(&st, parts, body).await; // Rule 5.
            }
            if accepts_sse(&parts.headers) {
                // Rule 6. The guard always inserts the identity; absence is a bug.
                let Some(identity) = parts.extensions.get::<VerifiedHttpIdentity>().cloned() else {
                    return json_error(StatusCode::UNAUTHORIZED, "Unauthorized");
                };
                return st.sse.open(identity);
            }
            json_error(
                StatusCode::NOT_ACCEPTABLE,
                "GET /mcp requires Accept: text/event-stream",
            )
        }
        Method::DELETE if header_sid => forward(&st, parts, body).await,
        _ => {
            let mut r = json_error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, POST, DELETE"));
            r
        }
    }
}

async fn health(State(st): State<Arc<AppState>>) -> Response {
    Json((st.health)()).into_response()
}

async fn instances(State(st): State<Arc<AppState>>) -> Response {
    Json(json!({ "instances": (st.instances)() })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_predicates() {
        assert!(has_modern_meta(
            &json!({"params":{"_meta":{META_VERSION_KEY: MODERN}}})
        ));
        assert!(!has_modern_meta(
            &json!({"params":{"_meta":{META_VERSION_KEY: "2025-11-25"}}})
        ));
        assert!(!has_modern_meta(&json!({"params":{}})));
        assert!(is_supported_version("2025-06-18"));
        assert!(!is_supported_version("1999-01-01"));
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json, Text/Event-Stream;q=1"),
        );
        assert!(accepts_sse(&h));
        assert_eq!(peer_key("::ffff:10.0.0.1".parse().expect("ip")), "10.0.0.1");
        assert_eq!(peer_key("::1".parse().expect("ip")), "::1");
    }
}
