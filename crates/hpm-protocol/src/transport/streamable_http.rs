//! Single-user HTTP listener: one axum app serving `/mcp` per plan §5.3 rules 1–8.
//!
//! - rmcp `StreamableHttpService` (legacy session mode) serves `2026-07-28`
//!   stateless requests and `2025-*` sessions.
//! - The legacy HTTP+SSE adapter ([`super::sse`]) shares `/mcp`.
//! - The front guard (rate limit → Host → Origin → bearer) runs before any body
//!   read or session allocation and inserts the verified identity.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::serve::IncomingStream;
use axum::{Json, Router};
use hpm_types::context::{Principal, VerifiedHttpIdentity};
use hyper::body::{Frame, Incoming, SizeHint};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use rmcp::model::ProtocolVersion;
use rmcp::transport::streamable_http_server::session::SessionManager;
use rmcp::transport::streamable_http_server::session::local::{LocalSessionManager, SessionConfig};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::Service;

use super::auth::{AuthFailureLimiter, operator_identity};
use super::json_error;
use super::limits::{HttpLimits, HttpSecurity, host_allowed, origin_allowed};
use super::sse::{
    Acquire, DEFAULT_KEEPALIVE, GlobalLimits, SESSION_REFUSED, SseSessions, json_rpc_request_key,
};
use crate::handler::McpHandler;

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const META_VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const MODERN: &str = "2026-07-28";
/// How often a Streamable session's watcher checks that rmcp still holds it.
const SESSION_POLL: Duration = Duration::from_secs(1);

/// Body builders for the 6.2.0 `/health` and `/instances` routes.
pub type JsonFn = Arc<dyn Fn() -> Value + Send + Sync>;

pub struct HttpConfig {
    pub security: HttpSecurity,
    pub limits: HttpLimits,
    pub keepalive: Duration,
    /// After shutdown, how long open connections may finish before being dropped.
    pub shutdown_grace: Duration,
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
            shutdown_grace: Duration::from_secs(10),
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
    limits: HttpLimits,
    shutdown_grace: Duration,
    /// Cancelled by `close_all_sessions`.
    stopping: CancellationToken,
    auth_failures: AuthFailureLimiter,
    rmcp: StreamableHttpService<McpHandler, LocalSessionManager>,
    rmcp_sessions: Arc<LocalSessionManager>,
    /// Streamable session id → the principal that initialized it (rule 5).
    bindings: Mutex<HashMap<String, Principal>>,
    /// `bindings.len()`, read synchronously by the SSE session cap.
    streamable_count: Arc<AtomicUsize>,
    /// Serializes session admission so the shared cap cannot be overshot.
    admit_lock: tokio::sync::Mutex<()>,
    global: Arc<GlobalLimits>,
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
            shutdown_grace,
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
        // 6.2.0 session timers apply to Streamable sessions too (review M2).
        let mut session_config = SessionConfig::default();
        session_config.keep_alive = Some(limits.session_idle_timeout);
        session_config.init_timeout = Some(limits.session_setup_timeout);
        let mut manager = LocalSessionManager::default();
        manager.session_config = session_config;
        let rmcp_sessions = Arc::new(manager);
        let rmcp = StreamableHttpService::new(
            move || Ok(streamable.clone()),
            rmcp_sessions.clone(),
            rmcp_config,
        );
        let global = Arc::new(GlobalLimits::new(&limits));
        let streamable_count = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(AppState {
            auth_failures: AuthFailureLimiter::new(
                limits.auth_failure_limit,
                limits.auth_failure_window,
                limits.max_tracked_auth_peers,
            ),
            sse: SseSessions::new(
                sse,
                limits.clone(),
                keepalive,
                global.clone(),
                streamable_count.clone(),
            ),
            limits,
            shutdown_grace,
            stopping: shutdown.clone(),
            security,
            rmcp,
            rmcp_sessions,
            bindings: Mutex::new(HashMap::new()),
            streamable_count,
            admit_lock: tokio::sync::Mutex::new(()),
            global,
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

    /// Refuses new sessions and requests, and closes every SSE and Streamable session.
    pub fn close_all_sessions(&self) {
        self.state.sse.close_all();
        self.shutdown.cancel();
    }

    pub fn sse_session_count(&self) -> usize {
        self.state.sse.session_count()
    }

    /// Live Streamable HTTP sessions known to the session cap.
    pub fn streamable_session_count(&self) -> usize {
        self.state.streamable_count.load(Ordering::Acquire)
    }

    /// Serves until `shutdown` resolves, then closes every session and waits up to
    /// `shutdown_grace` for connections before dropping them.
    ///
    /// Each connection gets a header read deadline (`headers_timeout`) before any
    /// authentication runs, and open connections are capped (`max_connections`).
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let router = self.router();
        let limits = &self.state.limits;
        let slots = Arc::new(Semaphore::new(limits.max_connections));
        let graceful = GracefulShutdown::new();
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(limits.headers_timeout);
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            let (stream, remote) = tokio::select! {
                r = listener.accept() => match r {
                    Ok(v) => v,
                    // Per-connection failures (e.g. reset before accept) are not fatal.
                    Err(_) => continue,
                },
                () = &mut shutdown => break,
            };
            while tasks.try_join_next().is_some() {}
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                drop(stream);
                continue;
            };
            let Ok(local) = stream.local_addr() else {
                continue;
            };
            let addrs = PeerAddrs { local, remote };
            let router = router.clone();
            let service = service_fn(move |mut req: hyper::Request<Incoming>| {
                req.extensions_mut().insert(ConnectInfo(addrs));
                let mut router = router.clone();
                async move { router.call(req).await }
            });
            let conn = graceful.watch(builder.serve_connection(TokioIo::new(stream), service));
            tasks.spawn(async move {
                // Connection errors (timeouts, resets) end only that connection.
                let _ = conn.await;
                drop(permit);
            });
        }
        self.close_all_sessions();
        let _ = tokio::time::timeout(self.state.shutdown_grace, graceful.shutdown()).await;
        tasks.abort_all();
        Ok(())
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

/// Holds a Streamable request's global POST slot and outstanding reservation until
/// its response body ends or is dropped (review M2).
struct RequestGuard {
    _permit: OwnedSemaphorePermit,
    reserved: Option<Arc<GlobalLimits>>,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Some(g) = self.reserved.take() {
            g.release(1);
        }
    }
}

struct GuardedBody {
    inner: Body,
    _guard: RequestGuard,
}

impl HttpBody for GuardedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

fn session_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Rule 5: a session bound to another principal is indistinguishable from an
/// unknown one. Ids we never bound are left to rmcp, which refuses them.
fn bound_elsewhere(st: &AppState, sid: &str, principal: &Principal) -> bool {
    st.bindings
        .lock()
        .map_or(true, |b| b.get(sid).is_some_and(|p| p != principal))
}

fn live_sessions(st: &AppState) -> usize {
    st.sse.session_count() + st.streamable_count.load(Ordering::Acquire)
}

/// Records the initializing principal and enforces the 6.2.0 lifetime cap; the
/// binding is dropped once rmcp no longer holds the session (idle, DELETE, close).
fn bind_session(st: &Arc<AppState>, sid: String, principal: Principal) {
    let Ok(mut b) = st.bindings.lock() else {
        return;
    };
    if b.insert(sid.clone(), principal).is_none() {
        st.streamable_count.fetch_add(1, Ordering::AcqRel);
    }
    drop(b);
    let st = st.clone();
    tokio::spawn(async move {
        let id: Arc<str> = sid.as_str().into();
        let lifetime = tokio::time::sleep(st.limits.session_max_lifetime);
        tokio::pin!(lifetime);
        loop {
            tokio::select! {
                () = &mut lifetime => break,
                () = st.stopping.cancelled() => break,
                () = tokio::time::sleep(SESSION_POLL) => {
                    if !st.rmcp_sessions.has_session(&id).await.unwrap_or(false) {
                        break;
                    }
                }
            }
        }
        let _ = st.rmcp_sessions.close_session(&id).await;
        if st
            .bindings
            .lock()
            .is_ok_and(|mut b| b.remove(&sid).is_some())
        {
            st.streamable_count.fetch_sub(1, Ordering::AcqRel);
        }
    });
}

fn too_many_sessions() -> Response {
    let mut r = json_error(StatusCode::TOO_MANY_REQUESTS, "Too many sessions");
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    r
}

async fn post(
    st: &Arc<AppState>,
    parts: axum::http::request::Parts,
    body: Body,
    query_sid: Option<String>,
    identity: VerifiedHttpIdentity,
) -> Response {
    // Node `requestTimeout`: the whole body within the deadline (review M1).
    let bytes = match tokio::time::timeout(
        st.limits.request_timeout,
        axum::body::to_bytes(body, st.limits.max_json_bytes),
    )
    .await
    {
        Err(_) => return json_error(StatusCode::REQUEST_TIMEOUT, "Request timeout"),
        Ok(Err(_)) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"),
        Ok(Ok(b)) => b,
    };
    let parsed = serde_json::from_slice::<Value>(&bytes);
    // Rule 7: legacy HTTP+SSE POST keeps 6.2.0 body errors.
    if let Some(sid) = &query_sid
        && !parsed.as_ref().is_ok_and(has_modern_meta)
    {
        return st.sse.post(sid, &identity, &parts.headers, bytes).await;
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
    if st.stopping.is_cancelled() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down");
    }
    // Rule 3: modern `_meta` → stateless; a stale session id never downgrades.
    let mut parts = parts;
    let modern = has_modern_meta(&value);
    if modern {
        parts.headers.remove(SESSION_HEADER);
    }
    let sid = session_header(&parts.headers);
    if let Some(sid) = &sid
        && bound_elsewhere(st, sid, &identity.principal)
    {
        return json_error(StatusCode::NOT_FOUND, SESSION_REFUSED);
    }
    let creates_session = sid.is_none()
        && !modern
        && value.get("method").and_then(Value::as_str) == Some("initialize");
    // Serialized so concurrent initializes cannot overshoot the shared cap.
    let _admit = if creates_session {
        let lock = st.admit_lock.lock().await;
        if live_sessions(st) >= st.limits.max_active_sessions {
            return too_many_sessions();
        }
        Some(lock)
    } else {
        None
    };
    let permit = match st.global.post_slots.acquire().await {
        Acquire::Granted(p) => p,
        Acquire::Full => {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many concurrent requests",
            );
        }
        Acquire::Closed => {
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down");
        }
    };
    let reserved = if json_rpc_request_key(&value).is_some() {
        if !st.global.reserve() {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many outstanding requests",
            );
        }
        Some(st.global.clone())
    } else {
        None
    };
    let guard = RequestGuard {
        _permit: permit,
        reserved,
    };
    let resp = forward(st, parts, Body::from(bytes)).await;
    if creates_session && let Some(new_sid) = session_header(resp.headers()) {
        bind_session(st, new_sid, identity.principal.clone());
    }
    let (parts, body) = resp.into_parts();
    Response::from_parts(
        parts,
        Body::new(GuardedBody {
            inner: body,
            _guard: guard,
        }),
    )
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
    // The guard always inserts the identity; absence is a bug, so fail closed.
    let Some(identity) = parts.extensions.get::<VerifiedHttpIdentity>().cloned() else {
        return json_error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let sid = session_header(&parts.headers);
    match parts.method {
        Method::POST => post(&st, parts, body, query_sid, identity).await,
        Method::GET | Method::DELETE
            if sid
                .as_deref()
                .is_some_and(|s| bound_elsewhere(&st, s, &identity.principal)) =>
        {
            json_error(StatusCode::NOT_FOUND, SESSION_REFUSED)
        }
        Method::GET => {
            if header_sid {
                return forward(&st, parts, body).await; // Rule 5.
            }
            if accepts_sse(&parts.headers) {
                return st.sse.open(identity); // Rule 6.
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
