//! Single-user HTTP listener: one axum app serving `/mcp` per plan §5.3 rules 1–8.
//!
//! - rmcp `StreamableHttpService` (legacy session mode) serves `2026-07-28`
//!   stateless requests and `2025-*` sessions.
//! - The legacy HTTP+SSE adapter ([`super::sse`]) shares `/mcp`.
//! - The front guard (rate limit → Host → Origin → bearer) runs before any body
//!   read or session allocation and inserts the verified identity.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
use hpm_types::context::{Principal, TransportKind, VerifiedHttpIdentity};
use hpm_types::limits::LimitExceeded;
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
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::Service;

use super::auth::{AuthFailureLimiter, AuthRequest, HttpAuthenticator, StaticBearerAuthenticator};
use super::expiry::StreamDeadline;
use super::json_error;
use super::limits::{
    HttpLimits, HttpSecurity, SessionAdmission, StreamKind, StreamOpen, StreamPermit, host_allowed,
    origin_allowed,
};
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

/// Body builder for the 6.2.0 `/health` route.
pub type JsonFn = Arc<dyn Fn() -> Value + Send + Sync>;

/// `/instances` list. `Err(())` means the list could not be loaded (the provider
/// logs why); the route then answers the 6.2.0 error boundary's 500
/// `{"error":"Internal server error"}`.
pub type InstancesFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Value, ()>> + Send>> + Send + Sync>;

/// `/instances` list for one verified identity (public seam PS-3). Same `Err(())`
/// meaning as [`InstancesFn`].
pub type PrincipalInstancesFn = Arc<
    dyn Fn(&VerifiedHttpIdentity) -> Pin<Box<dyn Future<Output = Result<Value, ()>> + Send>>
        + Send
        + Sync,
>;

pub struct HttpConfig {
    pub security: HttpSecurity,
    pub limits: HttpLimits,
    pub keepalive: Duration,
    /// After shutdown, how long open connections may finish before being dropped.
    pub shutdown_grace: Duration,
    /// `/health` body (`status`, `servicenow_instance`, `instance_name`, `timestamp`).
    pub health: JsonFn,
    /// `/instances` list; wrapped as `{"instances": …}`.
    pub instances: InstancesFn,
}

impl HttpConfig {
    pub fn new(security: HttpSecurity, health: JsonFn, instances: InstancesFn) -> Self {
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

/// The front guard's state: failure limiter, Host/Origin allow-lists and the
/// authenticator.
struct FrontDoor {
    allowed_hosts: BTreeSet<String>,
    allowed_origins: BTreeSet<String>,
    auth_failures: AuthFailureLimiter,
    authenticator: Arc<dyn HttpAuthenticator>,
    /// RFC 9728 body, served unauthenticated when the authenticator provides one.
    resource_metadata: Option<Value>,
}

/// A Streamable session's owner and its streams' credential deadline.
struct Binding {
    principal: Principal,
    deadline: StreamDeadline,
    /// PS-7: released when the binding is removed (session ended).
    _permit: Option<StreamPermit>,
}

struct AppState {
    front: FrontDoor,
    limits: HttpLimits,
    shutdown_grace: Duration,
    /// Cancelled by `close_all_sessions`.
    stopping: CancellationToken,
    rmcp: StreamableHttpService<McpHandler, LocalSessionManager>,
    rmcp_sessions: Arc<LocalSessionManager>,
    /// Streamable session id → the principal that initialized it (rule 5).
    bindings: Mutex<HashMap<String, Binding>>,
    /// `bindings.len()`, read synchronously by the SSE session cap.
    streamable_count: Arc<AtomicUsize>,
    /// Serializes session admission so the shared cap cannot be overshot.
    admit_lock: tokio::sync::Mutex<()>,
    global: Arc<GlobalLimits>,
    sse: Arc<SseSessions>,
    health: JsonFn,
    instances: InstancesFn,
    principal_instances: OnceLock<PrincipalInstancesFn>,
    admission: OnceLock<Arc<dyn SessionAdmission>>,
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
    /// Single-user listener: requests authenticate with the static bearer token
    /// in `config.security` ([`StaticBearerAuthenticator`]).
    pub fn new(streamable: McpHandler, sse: McpHandler, config: HttpConfig) -> Self {
        let authenticator = Arc::new(StaticBearerAuthenticator::new(
            config.security.token.clone(),
        ));
        Self::with_authenticator(streamable, sse, config, authenticator)
    }

    /// Listener whose requests authenticate with `authenticator` instead of the
    /// static bearer token (`config.security.token` is then unused). Every other
    /// front rule (failure limiter, Host/Origin, deadlines, connection caps,
    /// session-to-principal binding) is unchanged. When the authenticator returns
    /// [`HttpAuthenticator::protected_resource_metadata`], GET
    /// `/.well-known/oauth-protected-resource` serves it without authentication.
    pub fn with_authenticator(
        streamable: McpHandler,
        sse: McpHandler,
        config: HttpConfig,
        authenticator: Arc<dyn HttpAuthenticator>,
    ) -> Self {
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
            front: FrontDoor {
                auth_failures: AuthFailureLimiter::new(
                    limits.auth_failure_limit,
                    limits.auth_failure_window,
                    limits.max_tracked_auth_peers,
                ),
                resource_metadata: authenticator.protected_resource_metadata(),
                authenticator,
                allowed_hosts: security.allowed_hosts,
                allowed_origins: security.allowed_origins,
            },
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
            rmcp,
            rmcp_sessions,
            bindings: Mutex::new(HashMap::new()),
            streamable_count,
            admit_lock: tokio::sync::Mutex::new(()),
            global,
            health,
            instances,
            principal_instances: OnceLock::new(),
            admission: OnceLock::new(),
        });
        Self { state, shutdown }
    }

    /// `GET /instances` answers from `f` with the verified identity instead of the
    /// process-global [`InstancesFn`] (public seam PS-3). `Err(())` keeps the
    /// [`InstancesFn`] meaning (500 error boundary). Without this call the global
    /// function answers, unchanged. The setting is shared by every clone of this
    /// app and the first call wins.
    pub fn with_principal_instances(self, f: PrincipalInstancesFn) -> Self {
        let _ = self.state.principal_instances.set(f);
        self
    }

    /// Budgets sessions and long-lived streams per `admission` (public seam PS-7):
    /// legacy SSE sessions, Streamable sessions, a session's standalone `GET`
    /// stream and `2026-07-28` `subscriptions/listen` responses. A refusal answers
    /// `429` with `Retry-After` and creates nothing. Without this call nothing is
    /// checked. The setting is shared by every clone of this app and the first
    /// call wins.
    pub fn with_session_admission(self, admission: Arc<dyn SessionAdmission>) -> Self {
        let _ = self.state.admission.set(admission);
        self
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
    /// authentication runs and a pre-auth deadline (`preauth_timeout`); open
    /// connections are capped globally and per peer IP ([`ConnTable`]).
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let router = self.router();
        let limits = &self.state.limits;
        let conns = Arc::new(ConnTable::new(limits));
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
            let ip = peer_key(remote.ip());
            let Some(conn) = conns.admit(ip.clone()) else {
                drop(stream);
                continue;
            };
            let Ok(local) = stream.local_addr() else {
                conns.release(conn.id);
                continue;
            };
            let addrs = PeerAddrs { local, remote };
            let router = router.clone();
            let auth = conn.state.clone();
            let service = service_fn(move |mut req: hyper::Request<Incoming>| {
                req.extensions_mut().insert(ConnectInfo(addrs));
                req.extensions_mut().insert(ConnAuth(auth.clone()));
                let mut router = router.clone();
                async move { router.call(req).await }
            });
            let serving = graceful.watch(builder.serve_connection(TokioIo::new(stream), service));
            let conns = conns.clone();
            let preauth = limits.preauth_timeout;
            tasks.spawn(async move {
                let deadline = async {
                    tokio::time::sleep(preauth).await;
                    if conn.state.load(Ordering::Acquire) != AUTHED {
                        return;
                    }
                    std::future::pending::<()>().await;
                };
                // Connection errors (timeouts, resets) end only that connection;
                // eviction and the pre-auth deadline drop it (review F1).
                tokio::select! {
                    _ = serving => {}
                    () = conn.evict.cancelled() => {}
                    () = deadline => {}
                }
                conns.release(conn.id);
            });
        }
        self.close_all_sessions();
        let _ = tokio::time::timeout(self.state.shutdown_grace, graceful.shutdown()).await;
        tasks.abort_all();
        Ok(())
    }
}

/// Connection auth state shared by the table and the guard (reviews F1, L2).
const UNAUTH: u8 = 0;
const AUTHED: u8 = 1;
const EVICTED: u8 = 2;

#[derive(Clone)]
struct ConnAuth(Arc<AtomicU8>);

struct ConnEntry {
    id: u64,
    ip: String,
    state: Arc<AtomicU8>,
    evict: CancellationToken,
}

/// Open connections (review F1). Unauthenticated ones are capped per peer IP and,
/// at either cap, the oldest unauthenticated connection in scope is evicted for
/// the new one, so silent sockets cannot lock out an authenticated client. When
/// every open connection has authenticated, a new one over the global cap is
/// refused.
struct ConnTable {
    max: usize,
    unauth_per_peer: usize,
    state: Mutex<(u64, Vec<ConnEntry>)>,
}

struct Admitted {
    id: u64,
    state: Arc<AtomicU8>,
    evict: CancellationToken,
}

fn unauthenticated(c: &ConnEntry) -> bool {
    c.state.load(Ordering::Acquire) == UNAUTH
}

impl ConnTable {
    fn new(limits: &HttpLimits) -> Self {
        Self {
            max: limits.max_connections,
            unauth_per_peer: limits.max_unauthenticated_per_peer,
            state: Mutex::new((0, Vec::new())),
        }
    }

    fn admit(&self, ip: String) -> Option<Admitted> {
        let mut guard = self.state.lock().ok()?;
        let (next, open) = &mut *guard;
        if open
            .iter()
            .filter(|c| c.ip == ip && unauthenticated(c))
            .count()
            >= self.unauth_per_peer
        {
            Self::evict_oldest(open, Some(&ip));
        }
        if open.len() >= self.max && !Self::evict_oldest(open, None) {
            return None;
        }
        *next += 1;
        let entry = ConnEntry {
            id: *next,
            ip,
            state: Arc::new(AtomicU8::new(UNAUTH)),
            evict: CancellationToken::new(),
        };
        let admitted = Admitted {
            id: entry.id,
            state: entry.state.clone(),
            evict: entry.evict.clone(),
        };
        open.push(entry);
        Some(admitted)
    }

    /// Evicts the oldest unauthenticated connection (from `ip` if given).
    fn evict_oldest(open: &mut Vec<ConnEntry>, ip: Option<&str>) -> bool {
        // Review L2: claim the connection atomically; one that authenticated
        // concurrently fails the swap and is skipped.
        let Some(pos) = open.iter().position(|c| {
            ip.is_none_or(|ip| c.ip == ip)
                && c.state
                    .compare_exchange(UNAUTH, EVICTED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
        }) else {
            return false;
        };
        open.remove(pos).evict.cancel();
        true
    }

    fn release(&self, id: u64) {
        if let Ok(mut guard) = self.state.lock() {
            guard.1.retain(|c| c.id != id);
        }
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

/// Rate-limit and connection-cap key: the socket peer address only. Forwarding
/// headers (`X-Forwarded-For`, `Forwarded`) are never read, so they cannot be
/// spoofed; behind a reverse proxy every client shares the proxy's key. IPv6
/// is keyed by its /64, since one host routinely holds a whole /64 (review L3);
/// loopback and v4-mapped addresses keep the exact address.
fn peer_key(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                v4.to_string()
            } else if v6.is_loopback() {
                v6.to_string()
            } else {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        }
        v4 => v4.to_string(),
    }
}

async fn guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    front_guard(&st.front, req, next).await
}

/// RFC 9728 well-known path of the protected-resource metadata.
const RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

/// Failure limiter → Host → Origin → (metadata) → authenticator → eviction check;
/// inserts the verified identity.
async fn front_guard(front: &FrontDoor, mut req: Request, next: Next) -> Response {
    let Some(ConnectInfo(addrs)) = req.extensions().get::<ConnectInfo<PeerAddrs>>().copied() else {
        // Served without connect info: no Host or peer basis, fail closed.
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
    };
    let peer = peer_key(addrs.remote.ip());
    let now = Instant::now();
    if let Some(retry) = front.auth_failures.retry_after_seconds(&peer, now) {
        return too_many_failures(retry);
    }
    let h = req.headers();
    if !host_allowed(h, addrs.local, &front.allowed_hosts) {
        return json_error(StatusCode::FORBIDDEN, "Host not allowed");
    }
    if !origin_allowed(h, &front.allowed_origins) {
        return json_error(StatusCode::FORBIDDEN, "Origin not allowed");
    }
    if let Some(metadata) = &front.resource_metadata
        && req.method() == Method::GET
        && req.uri().path() == RESOURCE_METADATA_PATH
    {
        return Json(metadata.clone()).into_response();
    }
    let verdict = front
        .authenticator
        .authenticate(AuthRequest {
            headers: h,
            method: req.method(),
            path: req.uri().path(),
            peer: addrs.remote.ip(),
        })
        .await;
    let identity = match verdict {
        Ok(identity) => identity,
        Err(challenge) => {
            if challenge.counts_as_failure && !front.auth_failures.record_failure(&peer, now) {
                let window = front.auth_failures.window().as_millis().div_ceil(1000) as u64;
                return too_many_failures(window);
            }
            let msg = match challenge.status {
                StatusCode::UNAUTHORIZED => "Unauthorized",
                StatusCode::FORBIDDEN => "Forbidden",
                StatusCode::SERVICE_UNAVAILABLE => "Service unavailable",
                s => s.canonical_reason().unwrap_or("Error"),
            };
            let mut r = json_error(challenge.status, msg);
            r.headers_mut()
                .insert(header::WWW_AUTHENTICATE, challenge.www_authenticate);
            return r;
        }
    };
    // Review L2: the flag moves Unauth → Authed only if eviction has not won.
    if let Some(ConnAuth(state)) = req.extensions().get::<ConnAuth>()
        && let Err(EVICTED) =
            state.compare_exchange(UNAUTH, AUTHED, Ordering::AcqRel, Ordering::Acquire)
    {
        let mut r = json_error(StatusCode::SERVICE_UNAVAILABLE, "Connection evicted");
        r.headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
        return r;
    }
    req.extensions_mut().insert(identity);
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

/// A `_meta` protocol version we know and that is at least 2026-07-28; unknown
/// strings that merely sort later do not select stateless routing (review F2).
fn has_modern_meta(body: &Value) -> bool {
    body.get("params")
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get(META_VERSION_KEY))
        .and_then(Value::as_str)
        .is_some_and(|v| v >= MODERN && is_supported_version(v))
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
    /// PS-7 permit of a `subscriptions/listen` response, if any.
    _stream: Option<StreamPermit>,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Some(g) = self.reserved.take() {
            g.release(1);
        }
    }
}

struct GuardedBody<G> {
    inner: Body,
    _guard: G,
}

impl<G: Unpin> HttpBody for GuardedBody<G> {
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
/// unknown one. Every session rmcp creates is bound in `post`, so an id we never
/// bound is refused here rather than handed to rmcp (review F2).
fn bound_elsewhere(st: &AppState, sid: &str, principal: &Principal) -> bool {
    st.bindings.lock().map_or(true, |b| {
        b.get(sid).is_none_or(|s| &s.principal != principal)
    })
}

/// T-K02: the stream deadline of a session bound to `identity`'s principal,
/// refreshed by this request's identity. `None` for any other session.
fn session_deadline(
    st: &AppState,
    sid: &str,
    identity: &VerifiedHttpIdentity,
) -> Option<StreamDeadline> {
    let b = st.bindings.lock().ok()?;
    let s = b.get(sid).filter(|s| s.principal == identity.principal)?;
    s.deadline.refresh(identity);
    Some(s.deadline.clone())
}

fn live_sessions(st: &AppState) -> usize {
    st.sse.session_count() + st.streamable_count.load(Ordering::Acquire)
}

/// Records the initializing principal and enforces the 6.2.0 lifetime cap; the
/// binding is dropped once rmcp no longer holds the session (idle, DELETE, close).
fn bind_session(
    st: &Arc<AppState>,
    sid: String,
    principal: Principal,
    deadline: StreamDeadline,
    permit: Option<StreamPermit>,
) {
    let Ok(mut b) = st.bindings.lock() else {
        return;
    };
    if b.insert(
        sid.clone(),
        Binding {
            principal,
            deadline,
            _permit: permit,
        },
    )
    .is_none()
    {
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

/// PS-7: asks the configured [`SessionAdmission`] (if any) for a session or stream
/// slot; a refusal is answered with [`stream_refused`].
fn admit_stream(
    st: &AppState,
    identity: &VerifiedHttpIdentity,
    kind: StreamKind,
    transport: TransportKind,
) -> Result<Option<StreamPermit>, LimitExceeded> {
    let Some(admission) = st.admission.get() else {
        return Ok(None);
    };
    admission
        .open(StreamOpen::new(&identity.principal, kind, transport))
        .map(Some)
}

/// LIM-S-07: 429, `Retry-After` in whole seconds (at least 1, 1 when unknown) and a
/// body that carries only the scope word.
fn stream_refused(e: &LimitExceeded) -> Response {
    let secs = e
        .retry_after
        .map_or(1, |d| d.as_millis().div_ceil(1000).max(1));
    let mut r = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "error": "RATE_LIMITED", "scope": e.scope })),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
        r.headers_mut().insert(header::RETRY_AFTER, v);
    }
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
        // UnsupportedProtocolVersionError: `data` echoes the request and lists ours.
        let data = json!({
            "requested": String::from_utf8_lossy(v.as_bytes()),
            "supported": ProtocolVersion::KNOWN_VERSIONS,
        });
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "jsonrpc": "2.0", "id": id, "error": {
                "code": -32022, "message": "Unsupported protocol version", "data": data,
            } })),
        )
            .into_response();
    }
    if st.stopping.is_cancelled() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down");
    }
    // C11: batches exist only in `2025-03-26` and rmcp cannot decode them; they
    // are rejected as one JSON-RPC Invalid Request rather than a bare 415.
    if value.is_array() {
        return jsonrpc_error(Value::Null, -32600, "Batch requests are not supported");
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
    let session = sid
        .as_deref()
        .and_then(|sid| session_deadline(st, sid, &identity));
    // Review F2: rmcp opens a session for every initialize, `_meta` or not, so
    // each one is admitted against the cap and bound.
    let method = value.get("method").and_then(Value::as_str);
    let creates_session = sid.is_none() && method == Some("initialize");
    // PS-7: a `2026-07-28` `subscriptions/listen` holds its response open
    // indefinitely; its permit lives with the response body.
    let stream_permit = if modern && method == Some("subscriptions/listen") {
        match admit_stream(
            st,
            &identity,
            StreamKind::ListenStream,
            TransportKind::StreamableHttp,
        ) {
            Ok(p) => p,
            Err(e) => return stream_refused(&e),
        }
    } else {
        None
    };
    // Serialized so concurrent initializes cannot overshoot the shared cap.
    let mut session_permit = None;
    let _admit = if creates_session {
        let lock = st.admit_lock.lock().await;
        if live_sessions(st) >= st.limits.max_active_sessions {
            return too_many_sessions();
        }
        // PS-7: kept with the binding; dropped here when rmcp opens no session.
        session_permit = match admit_stream(
            st,
            &identity,
            StreamKind::StreamableSession,
            TransportKind::StreamableHttp,
        ) {
            Ok(p) => p,
            Err(e) => return stream_refused(&e),
        };
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
        _stream: stream_permit,
    };
    let resp = forward(st, parts, Body::from(bytes)).await;
    // T-K02: a session's streams share its deadline; a stateless response gets one
    // from this request's identity.
    let deadline = session.unwrap_or_else(|| StreamDeadline::new(&identity));
    if creates_session && let Some(new_sid) = session_header(resp.headers()) {
        bind_session(
            st,
            new_sid,
            identity.principal.clone(),
            deadline.clone(),
            session_permit.take(),
        );
    }
    hold(deadline.bound(resp), guard)
}

/// Keeps `guard` alive until `resp`'s body ends or is dropped.
fn hold<G: Send + Unpin + 'static>(resp: Response, guard: G) -> Response {
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
                // Rule 5. A session's standalone stream (or its resume) is a
                // long-lived stream: budgeted per principal (PS-7).
                let deadline = sid
                    .as_deref()
                    .and_then(|sid| session_deadline(&st, sid, &identity));
                let stream = match admit_stream(
                    &st,
                    &identity,
                    StreamKind::ListenStream,
                    TransportKind::StreamableHttp,
                ) {
                    Ok(p) => p,
                    Err(e) => return stream_refused(&e),
                };
                let resp = forward(&st, parts, body).await;
                let resp = match deadline {
                    Some(d) => d.bound(resp),
                    None => resp,
                };
                return match stream {
                    Some(p) => hold(resp, p),
                    None => resp,
                };
            }
            if accepts_sse(&parts.headers) {
                // Rule 6.
                let permit = match admit_stream(
                    &st,
                    &identity,
                    StreamKind::LegacySseSession,
                    TransportKind::LegacySse,
                ) {
                    Ok(p) => p,
                    Err(e) => return stream_refused(&e),
                };
                return st.sse.open_with(identity, permit);
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

async fn instances(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let listed = match st.principal_instances.get() {
        // The guard always inserts the identity; absence is a bug, so fail closed.
        Some(per_principal) => match req.extensions().get::<VerifiedHttpIdentity>() {
            Some(identity) => per_principal(identity).await,
            None => return json_error(StatusCode::UNAUTHORIZED, "Unauthorized"),
        },
        None => (st.instances)().await,
    };
    match listed {
        Ok(list) => Json(json!({ "instances": list })).into_response(),
        Err(()) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "Internal server error" })),
        )
            .into_response(),
    }
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
        assert!(!has_modern_meta(
            &json!({"params":{"_meta":{META_VERSION_KEY: "9999-01-01"}}})
        ));
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
        // Review L3: one /64 is one key.
        let a = peer_key("2001:db8:1:2::1".parse().expect("ip"));
        assert_eq!(a, "2001:db8:1:2::/64");
        assert_eq!(
            a,
            peer_key("2001:db8:1:2:ffff:ffff:ffff:ffff".parse().expect("ip"))
        );
        assert_ne!(a, peer_key("2001:db8:1:3::1".parse().expect("ip")));
    }

    use std::sync::atomic::AtomicU32;

    use axum::body::to_bytes;
    use hpm_types::BoxFut;
    use hpm_types::context::ClientIdentity;
    use tower::ServiceExt;

    use crate::transport::auth::{API_TOKEN_CLIENT_ID, ApiToken, AuthChallenge, operator_identity};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const LOCAL: &str = "127.0.0.1:3000";
    const METADATA_URL: &str = "https://mcp.example/.well-known/oauth-protected-resource";

    /// Accepts `Bearer good`, answers 403 for `Bearer forbidden`, else 401 with
    /// `resource_metadata`; counts its calls.
    struct Custom {
        calls: AtomicU32,
    }

    fn federated() -> VerifiedHttpIdentity {
        VerifiedHttpIdentity::new(
            Principal::Federated {
                org: "acme".into(),
                iss: "https://idp.example".into(),
                sub: "alice".into(),
            },
            ClientIdentity {
                client_id: "custom".into(),
                verified: true,
            },
            vec!["scope:mcp:tools.read".to_string()].into(),
        )
    }

    impl HttpAuthenticator for Custom {
        fn authenticate<'a>(
            &'a self,
            req: AuthRequest<'a>,
        ) -> BoxFut<'a, Result<VerifiedHttpIdentity, AuthChallenge>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let auth = req.headers.get(header::AUTHORIZATION).cloned();
            let peer_ok = req.peer.is_loopback() && req.path == "/probe";
            Box::pin(async move {
                match auth.as_ref().and_then(|v| v.to_str().ok()) {
                    Some("Bearer good") if peer_ok => Ok(federated()),
                    Some("Bearer forbidden") => Err(AuthChallenge {
                        status: StatusCode::FORBIDDEN,
                        www_authenticate: HeaderValue::from_static(
                            "Bearer error=\"insufficient_scope\"",
                        ),
                        counts_as_failure: false,
                    }),
                    _ => Err(AuthChallenge::unauthorized(Some(METADATA_URL))),
                }
            })
        }

        fn protected_resource_metadata(&self) -> Option<Value> {
            Some(json!({"resource": "https://mcp.example", "bearer_methods_supported": ["header"]}))
        }
    }

    fn front(authenticator: Arc<dyn HttpAuthenticator>, failure_limit: u32) -> Arc<FrontDoor> {
        Arc::new(FrontDoor {
            allowed_hosts: BTreeSet::new(),
            allowed_origins: BTreeSet::new(),
            auth_failures: AuthFailureLimiter::new(failure_limit, Duration::from_secs(60), 16),
            resource_metadata: authenticator.protected_resource_metadata(),
            authenticator,
        })
    }

    /// `/probe` echoes the principal the guard inserted.
    fn app(front: Arc<FrontDoor>) -> Router {
        async fn probe(req: Request) -> Response {
            match req.extensions().get::<VerifiedHttpIdentity>() {
                Some(id) => format!("{:?}|{}", id.principal, id.client.client_id).into_response(),
                None => StatusCode::IM_A_TEAPOT.into_response(),
            }
        }
        async fn layer(State(f): State<Arc<FrontDoor>>, req: Request, next: Next) -> Response {
            front_guard(&f, req, next).await
        }
        Router::new()
            .route("/probe", any(probe))
            .layer(middleware::from_fn_with_state(front, layer))
    }

    async fn send(app: &Router, method: Method, path: &str, auth: Option<&str>) -> Response {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, LOCAL);
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        let mut req = req.body(Body::empty()).expect("request");
        let local: SocketAddr = LOCAL.parse().expect("addr");
        req.extensions_mut().insert(ConnectInfo(PeerAddrs {
            local,
            remote: "127.0.0.1:50000".parse().expect("addr"),
        }));
        app.clone().oneshot(req).await.expect("infallible")
    }

    async fn body(r: Response) -> String {
        String::from_utf8(
            to_bytes(r.into_body(), 1 << 16)
                .await
                .expect("body")
                .to_vec(),
        )
        .expect("utf8")
    }

    fn www(r: &Response) -> Option<&str> {
        r.headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
    }

    #[tokio::test]
    async fn static_bearer_default_keeps_single_user_behaviour() {
        let token = ApiToken::parse(Some(TOKEN)).expect("token");
        let app = app(front(
            Arc::new(StaticBearerAuthenticator::new(token.clone())),
            2,
        ));

        let ok = send(
            &app,
            Method::POST,
            "/probe",
            Some(&format!("Bearer {TOKEN}")),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        let expected = operator_identity(&token);
        assert_eq!(
            body(ok).await,
            format!("{:?}|{API_TOKEN_CLIENT_ID}", expected.principal)
        );

        // No metadata route in single-user: the path needs auth like any other.
        let meta = send(&app, Method::GET, RESOURCE_METADATA_PATH, None).await;
        assert_eq!(meta.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(www(&meta), Some("Bearer"));
        assert_eq!(body(meta).await, r#"{"error":"Unauthorized"}"#);

        let bad = send(&app, Method::POST, "/probe", Some("Bearer nope")).await;
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(www(&bad), Some("Bearer"));
        // Third failure trips the limiter (limit 2), then the peer is locked out.
        let limited = send(&app, Method::POST, "/probe", None).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            limited
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("60")
        );
        let ok_but_locked = send(
            &app,
            Method::POST,
            "/probe",
            Some(&format!("Bearer {TOKEN}")),
        )
        .await;
        assert_eq!(ok_but_locked.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn custom_authenticator_principal_reaches_extensions() {
        let custom = Arc::new(Custom {
            calls: AtomicU32::new(0),
        });
        let app = app(front(custom.clone(), 5));
        let ok = send(&app, Method::POST, "/probe", Some("Bearer good")).await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(
            body(ok).await,
            format!("{:?}|custom", federated().principal)
        );
        assert_eq!(custom.calls.load(Ordering::SeqCst), 1);

        // Metadata is served before (and without) authentication.
        let meta = send(&app, Method::GET, RESOURCE_METADATA_PATH, None).await;
        assert_eq!(meta.status(), StatusCode::OK);
        let meta: Value = serde_json::from_str(&body(meta).await).expect("json");
        assert_eq!(meta["resource"], "https://mcp.example");
        assert_eq!(custom.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn custom_authenticator_rejections() {
        let custom = Arc::new(Custom {
            calls: AtomicU32::new(0),
        });
        let app = app(front(custom.clone(), 1));

        // 403 does not count toward the limiter.
        for _ in 0..3 {
            let r = send(&app, Method::POST, "/probe", Some("Bearer forbidden")).await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
            assert_eq!(www(&r), Some("Bearer error=\"insufficient_scope\""));
            assert_eq!(body(r).await, r#"{"error":"Forbidden"}"#);
        }

        let r = send(&app, Method::POST, "/probe", None).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            www(&r),
            Some(format!("Bearer resource_metadata=\"{METADATA_URL}\"").as_str())
        );
        // The 401 counted: the peer is now limited before the authenticator runs.
        let r = send(&app, Method::POST, "/probe", None).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(custom.calls.load(Ordering::SeqCst), 4);
    }
}
