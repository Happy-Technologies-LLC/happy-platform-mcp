//! Public seams PS-2 (list-changed), PS-3 (per-principal `/instances`) and PS-7
//! (session and stream admission), 7.2.0. SEAM-T-02, SEAM-T-03, SEAM-T-07.
//!
//! In-process router and duplex stdio; real time (every wait is bounded by 5 s).

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Request, Response, StatusCode, header};
use hpm_protocol::McpHandler;
use hpm_protocol::transport::limits::HttpSecurity;
use hpm_protocol::transport::{
    AuthChallenge, AuthRequest, HttpApp, HttpAuthenticator, HttpConfig, PeerAddrs,
    PrincipalInstancesFn, SessionAdmission, StreamKind, StreamOpen, StreamPermit,
};
use hpm_tools::dispatcher::{Dispatcher, RequiredPropertiesValidator, Services};
use hpm_tools::registration::ToolRegistry;
use hpm_tools::single_user::{
    AllowAll, LocalOnlyCredentials, PassThrough, StderrAudit, UnloadedClassification,
};
use hpm_types::BoxFut;
use hpm_types::context::{
    AdmissionKey, ClientIdentity, DeploymentMode, Principal, TrustedRoot, VerifiedHttpIdentity,
};
use hpm_types::limits::LimitExceeded;
use hpm_types::operation::ScriptOperandGuard;
use hpm_types::policy::ListChangeSource;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::Service;

const LOCAL: &str = "127.0.0.1:3000";
const LEGACY: &str = "2024-11-05";
const SESSION: &str = "2025-11-25";
const MODERN: &str = "2026-07-28";
const BOTH: &str = "application/json, text/event-stream";
const WITHIN: Duration = Duration::from_secs(5);

static KEY: LazyLock<AdmissionKey> =
    LazyLock::new(|| AdmissionKey::mint().expect("first mint in this process"));

/// `Bearer <sub>`: a federated principal named `<sub>`, no expiry.
struct Subject;

impl HttpAuthenticator for Subject {
    fn authenticate<'a>(
        &'a self,
        req: AuthRequest<'a>,
    ) -> BoxFut<'a, Result<VerifiedHttpIdentity, AuthChallenge>> {
        let sub = req
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string);
        Box::pin(async move {
            let Some(sub) = sub else {
                return Err(AuthChallenge::unauthorized(None));
            };
            Ok(VerifiedHttpIdentity::new(
                Principal::Federated {
                    org: "acme".into(),
                    iss: "https://idp.example".into(),
                    sub,
                },
                ClientIdentity {
                    client_id: "c".into(),
                    verified: true,
                },
                Vec::new().into(),
            ))
        })
    }
}

fn dispatcher() -> Arc<Dispatcher> {
    let services = Services {
        validator: Arc::new(RequiredPropertiesValidator),
        classification: Arc::new(UnloadedClassification),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: Arc::new(LocalOnlyCredentials),
        audit: Arc::new(StderrAudit),
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
        usage: None,
    };
    Arc::new(Dispatcher::new(
        ToolRegistry::new().expect("registry"),
        services,
        *KEY,
    ))
}

// ------------------------------------------------------------ list-change source

/// A source whose epoch the test bumps; counts the tasks parked in `changed`.
#[derive(Default)]
struct Stub {
    epoch: AtomicU64,
    wake: tokio::sync::Notify,
    waiting: AtomicUsize,
}

impl Stub {
    fn bump(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_waiters();
    }
}

struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ListChangeSource for Stub {
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn changed<'a>(&'a self, seen: u64) -> BoxFut<'a, ()> {
        Box::pin(async move {
            self.waiting.fetch_add(1, Ordering::SeqCst);
            let _waiting = Waiting(&self.waiting);
            loop {
                let notified = self.wake.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.epoch.load(Ordering::SeqCst) > seen {
                    return;
                }
                notified.await;
            }
        })
    }
}

/// Polls `cond` until it holds, panicking after `WITHIN`.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let end = Instant::now() + WITHIN;
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn handlers(source: Option<Arc<Stub>>) -> (McpHandler, McpHandler) {
    let d = dispatcher();
    let build = |root| {
        let b = McpHandler::builder(root, d.clone());
        match &source {
            Some(s) => b.list_changes(s.clone()).build(),
            None => b.build(),
        }
    };
    (
        build(TrustedRoot::streamable_http(DeploymentMode::MultiUser)),
        build(TrustedRoot::legacy_sse(DeploymentMode::MultiUser)),
    )
}

fn config() -> HttpConfig {
    let mut config = HttpConfig::new(
        HttpSecurity::new(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            &[],
            &[],
        )
        .expect("security"),
        Arc::new(|| json!({})),
        Arc::new(|| Box::pin(async { Ok(json!([{"name": "global"}])) })),
    );
    config.keepalive = Duration::from_secs(3600);
    config
}

fn app(source: Option<Arc<Stub>>) -> HttpApp {
    let (streamable, sse) = handlers(source);
    HttpApp::with_authenticator(streamable, sse, config(), Arc::new(Subject))
}

// --------------------------------------------------------------------- HTTP I/O

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    auth: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Response<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, LOCAL)
        .header(header::AUTHORIZATION, format!("Bearer {auth}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let mut req = req
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .expect("request");
    let local: SocketAddr = LOCAL.parse().expect("addr");
    req.extensions_mut().insert(ConnectInfo(PeerAddrs {
        local,
        remote: "127.0.0.1:50000".parse().expect("addr"),
    }));
    app.clone().call(req).await.expect("infallible")
}

/// Reads frames until `needle` appears; panics if the stream ends or stalls.
async fn read_until(body: &mut Body, needle: &str) -> String {
    let mut seen = String::new();
    tokio::time::timeout(WITHIN, async {
        while !seen.contains(needle) {
            let frame = std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx))
                .await
                .unwrap_or_else(|| panic!("stream ended before {needle:?}: {seen}"))
                .expect("frame");
            if let Ok(data) = frame.into_data() {
                seen.push_str(&String::from_utf8_lossy(&data));
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}: {seen}"));
    seen
}

async fn body_text(r: Response<Body>) -> String {
    let bytes = axum::body::to_bytes(r.into_body(), 1 << 20)
        .await
        .expect("body");
    String::from_utf8_lossy(&bytes).into_owned()
}

fn initialize(version: &str) -> Value {
    json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
        "protocolVersion": version, "capabilities": {}, "clientInfo": {"name":"t","version":"1"}}})
}

fn initialized() -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/initialized"})
}

fn session_headers(sid: &str) -> [(&str, &str); 3] {
    [
        ("Accept", BOTH),
        ("mcp-session-id", sid),
        ("mcp-protocol-version", SESSION),
    ]
}

/// `initialize` on `/mcp`; the response (SSE) is left unread.
async fn streamable_initialize(app: &Router, auth: &str) -> Response<Body> {
    send(
        app,
        "POST",
        "/mcp",
        auth,
        &[("Accept", BOTH)],
        Some(&initialize(SESSION)),
    )
    .await
}

fn sid_of(r: &Response<Body>) -> String {
    r.headers()
        .get("mcp-session-id")
        .and_then(|v: &HeaderValue| v.to_str().ok())
        .expect("session id")
        .to_string()
}

/// An initialized Streamable `2025-11-25` session; returns its id and init reply.
async fn streamable_session(app: &Router, auth: &str) -> (String, String) {
    let r = streamable_initialize(app, auth).await;
    assert_eq!(r.status(), StatusCode::OK);
    let sid = sid_of(&r);
    let mut body = r.into_body();
    let init = read_until(&mut body, "serverInfo").await;
    let r = send(
        app,
        "POST",
        "/mcp",
        auth,
        &session_headers(&sid),
        Some(&initialized()),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    (sid, init)
}

async fn streamable_get(app: &Router, sid: &str, auth: &str) -> Response<Body> {
    send(
        app,
        "GET",
        "/mcp",
        auth,
        &[
            ("Accept", "text/event-stream"),
            ("mcp-session-id", sid),
            ("mcp-protocol-version", SESSION),
        ],
        None,
    )
    .await
}

/// Opens a legacy `2024-11-05` SSE session and initializes it; returns the stream,
/// the session id and the init reply.
async fn legacy_session(app: &Router, auth: &str) -> (Body, String, String) {
    let r = send(
        app,
        "GET",
        "/mcp",
        auth,
        &[("Accept", "text/event-stream")],
        None,
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body();
    let seen = read_until(&mut body, "sessionId=").await;
    let tail = &seen[seen.find("sessionId=").expect("id") + "sessionId=".len()..];
    let sid = tail.split(['\r', '\n']).next().expect("id").to_string();
    let path = format!("/mcp?sessionId={sid}");
    let r = send(app, "POST", &path, auth, &[], Some(&initialize(LEGACY))).await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let init = read_until(&mut body, "serverInfo").await;
    let r = send(app, "POST", &path, auth, &[], Some(&initialized())).await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    (body, sid, init)
}

async fn legacy_open(app: &Router, auth: &str) -> Response<Body> {
    send(
        app,
        "GET",
        "/mcp",
        auth,
        &[("Accept", "text/event-stream")],
        None,
    )
    .await
}

fn modern_listen(
    auth_headers: &[(&'static str, &'static str)],
) -> (Value, Vec<(&'static str, &'static str)>) {
    let body = json!({"jsonrpc":"2.0","id":1,"method":"subscriptions/listen","params":{
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": MODERN,
            "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {}
        },
        "notifications": {"toolsListChanged": true, "resourcesListChanged": true}}});
    let mut headers = vec![
        ("Accept", BOTH),
        ("MCP-Protocol-Version", MODERN),
        ("Mcp-Method", "subscriptions/listen"),
    ];
    headers.extend_from_slice(auth_headers);
    (body, headers)
}

async fn listen(app: &Router, auth: &str) -> Response<Body> {
    let (body, headers) = modern_listen(&[]);
    send(app, "POST", "/mcp", auth, &headers, Some(&body)).await
}

const TOOLS_CHANGED: &str = "notifications/tools/list_changed";
const RESOURCES_CHANGED: &str = "notifications/resources/list_changed";

// ============================================================== SEAM-T-02 (PS-2)

#[tokio::test]
async fn t02_default_advertises_nothing_and_listen_is_not_found() {
    let app = app(None);
    let router = app.router();
    let (_, init) = streamable_session(&router, "alice").await;
    assert!(!init.contains("listChanged"), "{init}");
    let (_, _, init) = legacy_session(&router, "alice").await;
    assert!(!init.contains("listChanged"), "{init}");

    // 7.1.0: rmcp answers an unaccepted listen `-32601` (HTTP 404), no stream.
    let r = listen(&router, "alice").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let seen = body_text(r).await;
    assert!(seen.contains("-32601"), "{seen}");
    assert!(!seen.contains("list_changed"), "{seen}");
}

#[tokio::test]
async fn t02_listen_stream_is_notified_and_its_end_stops_the_wait() {
    let stub = Arc::new(Stub::default());
    let router = app(Some(stub.clone())).router();

    let r = listen(&router, "alice").await;
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body();
    // The stream is up once the listener waits on the source.
    eventually("listen to wait", || {
        stub.waiting.load(Ordering::SeqCst) == 1
    })
    .await;
    stub.bump();
    let seen = read_until(&mut body, RESOURCES_CHANGED).await;
    let seen = if seen.contains(TOOLS_CHANGED) {
        seen
    } else {
        seen + &read_until(&mut body, TOOLS_CHANGED).await
    };
    assert!(seen.contains(TOOLS_CHANGED) && seen.contains(RESOURCES_CHANGED));

    // Closing the stream ends the listen task: nothing waits on the source any more.
    drop(body);
    eventually("listen task to end", || {
        stub.waiting.load(Ordering::SeqCst) == 0
    })
    .await;
}

#[tokio::test]
async fn t02_listen_only_sends_requested_categories() {
    let stub = Arc::new(Stub::default());
    let router = app(Some(stub.clone())).router();
    let (mut body, headers) = modern_listen(&[]);
    body["params"]["notifications"] = json!({"toolsListChanged": true});
    let r = send(&router, "POST", "/mcp", "alice", &headers, Some(&body)).await;
    let mut stream = r.into_body();
    eventually("listen to wait", || {
        stub.waiting.load(Ordering::SeqCst) == 1
    })
    .await;
    stub.bump();
    let seen = read_until(&mut stream, TOOLS_CHANGED).await;
    assert!(!seen.contains(RESOURCES_CHANGED), "{seen}");
}

#[tokio::test]
async fn t02_streamable_session_is_notified_and_a_closed_peer_ends_its_task() {
    let stub = Arc::new(Stub::default());
    let router = app(Some(stub.clone())).router();
    let (sid, init) = streamable_session(&router, "alice").await;
    assert!(init.contains("\"listChanged\":true"), "{init}");

    let get = streamable_get(&router, &sid, "alice").await;
    assert_eq!(get.status(), StatusCode::OK);
    let mut stream = get.into_body();
    eventually("push task to wait", || {
        stub.waiting.load(Ordering::SeqCst) == 1
    })
    .await;
    stub.bump();
    read_until(&mut stream, TOOLS_CHANGED).await;
    read_until(&mut stream, RESOURCES_CHANGED).await;

    // DELETE tears the session down; its push task ends within the peer poll.
    let r = send(
        &router,
        "DELETE",
        "/mcp",
        "alice",
        &session_headers(&sid),
        None,
    )
    .await;
    assert!(r.status().is_success(), "{}", r.status());
    ends(&stub).await;
}

#[tokio::test]
async fn t02_legacy_sse_session_is_notified_and_a_closed_peer_ends_its_task() {
    let stub = Arc::new(Stub::default());
    let router = app(Some(stub.clone())).router();
    let (mut stream, _, init) = legacy_session(&router, "alice").await;
    assert!(init.contains("\"listChanged\":true"), "{init}");
    eventually("push task to wait", || {
        stub.waiting.load(Ordering::SeqCst) == 1
    })
    .await;
    stub.bump();
    read_until(&mut stream, TOOLS_CHANGED).await;
    read_until(&mut stream, RESOURCES_CHANGED).await;

    drop(stream);
    ends(&stub).await;
}

/// The wait count reaches zero and stays there across a full peer poll.
async fn ends(stub: &Stub) {
    eventually("push task to end", || {
        stub.waiting.load(Ordering::SeqCst) == 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(stub.waiting.load(Ordering::SeqCst), 0, "task restarted");
}

// -------------------------------------------------------------------- stdio

struct Stdio {
    tx: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    init: String,
}

impl Stdio {
    async fn next(&mut self) -> String {
        tokio::time::timeout(WITHIN, self.lines.next_line())
            .await
            .expect("line in time")
            .expect("read")
            .expect("line")
    }

    async fn until(&mut self, needle: &str) {
        loop {
            let line = self.next().await;
            if line.contains(needle) {
                return;
            }
        }
    }
}

async fn stdio(source: Option<Arc<Stub>>) -> Stdio {
    let d = dispatcher();
    let b = McpHandler::builder(TrustedRoot::single_user_stdio(), d);
    let handler = match source {
        Some(s) => b.list_changes(s).build(),
        None => b.build(),
    };
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (server_read, server_write) = tokio::io::split(server_io);
    tokio::spawn(hpm_protocol::stdio::serve_io(
        handler,
        server_read,
        server_write,
    ));
    let (client_read, mut tx) = tokio::io::split(client_io);
    let mut lines = BufReader::new(client_read).lines();
    tx.write_all(format!("{}\n", initialize(SESSION)).as_bytes())
        .await
        .expect("write");
    let init = tokio::time::timeout(WITHIN, lines.next_line())
        .await
        .expect("init in time")
        .expect("read")
        .expect("line");
    tx.write_all(format!("{}\n", initialized()).as_bytes())
        .await
        .expect("write");
    Stdio { tx, lines, init }
}

#[tokio::test]
async fn t02_stdio_default_advertises_nothing() {
    let s = stdio(None).await;
    assert!(!s.init.contains("listChanged"), "{}", s.init);
}

#[tokio::test]
async fn t02_stdio_is_notified_and_a_closed_peer_ends_its_task() {
    let stub = Arc::new(Stub::default());
    let mut s = stdio(Some(stub.clone())).await;
    assert!(s.init.contains("\"listChanged\":true"), "{}", s.init);
    eventually("push task to wait", || {
        stub.waiting.load(Ordering::SeqCst) == 1
    })
    .await;
    stub.bump();
    s.until(TOOLS_CHANGED).await;
    s.until(RESOURCES_CHANGED).await;

    let Stdio { tx, lines, .. } = s;
    drop(tx);
    drop(lines);
    ends(&stub).await;
}

// ============================================================== SEAM-T-03 (PS-3)

#[tokio::test]
async fn t03_instances_default_is_the_global_closure() {
    let router = app(None).router();
    for who in ["alice", "bob"] {
        let r = send(&router, "GET", "/instances", who, &[], None).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body_text(r).await, r#"{"instances":[{"name":"global"}]}"#);
    }
}

#[tokio::test]
async fn t03_principal_closure_receives_the_verified_identity() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let f: PrincipalInstancesFn = Arc::new(move |identity| {
        let sub = match &identity.principal {
            Principal::Federated { sub, .. } => sub.clone(),
            other => format!("{other:?}"),
        };
        log.lock().expect("log").push(sub.clone());
        Box::pin(async move {
            if sub == "broken" {
                Err(())
            } else {
                Ok(json!([{ "name": format!("for-{sub}") }]))
            }
        })
    });
    let router = app(None).with_principal_instances(f).router();

    for who in ["alice", "bob"] {
        let r = send(&router, "GET", "/instances", who, &[], None).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            body_text(r).await,
            format!(r#"{{"instances":[{{"name":"for-{who}"}}]}}"#)
        );
    }
    // `Err(())` keeps the `InstancesFn` meaning: the 500 error boundary.
    let r = send(&router, "GET", "/instances", "broken", &[], None).await;
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_text(r).await, r#"{"error":"Internal server error"}"#);
    // Unauthenticated callers never reach the closure.
    let r = Request::builder()
        .uri("/instances")
        .header(header::HOST, LOCAL)
        .body(Body::empty())
        .expect("request");
    let mut r = r;
    r.extensions_mut().insert(ConnectInfo(PeerAddrs {
        local: LOCAL.parse().expect("addr"),
        remote: "127.0.0.1:50000".parse().expect("addr"),
    }));
    let r = router.clone().call(r).await.expect("infallible");
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(*seen.lock().expect("log"), ["alice", "bob", "broken"]);
}

// ============================================================== SEAM-T-07 (PS-7)

/// At most `cap` live permits per principal subject; records every request.
struct Cap {
    cap: usize,
    live: Arc<AtomicUsize>,
    opens: std::sync::Mutex<Vec<(StreamKind, String)>>,
}

struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Cap {
    fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            cap,
            live: Arc::new(AtomicUsize::new(0)),
            opens: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }
}

impl SessionAdmission for Cap {
    fn open(&self, req: StreamOpen<'_>) -> Result<StreamPermit, LimitExceeded> {
        self.opens
            .lock()
            .expect("opens")
            .push((req.kind, format!("{:?}", req.transport)));
        if self.live.fetch_add(1, Ordering::SeqCst) >= self.cap {
            self.live.fetch_sub(1, Ordering::SeqCst);
            return Err(LimitExceeded::new(
                "limit_streams",
                "streams",
                Some(Duration::from_millis(1500)),
            ));
        }
        Ok(StreamPermit::new(Slot(self.live.clone())))
    }
}

async fn assert_refused(r: Response<Body>) {
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    // 1.5 s rounds up to whole seconds.
    assert_eq!(r.headers().get(header::RETRY_AFTER).expect("header"), "2");
    assert!(r.headers().get("mcp-session-id").is_none());
    assert_eq!(
        body_text(r).await,
        r#"{"error":"RATE_LIMITED","scope":"streams"}"#
    );
}

#[tokio::test]
async fn t07_default_listener_admits_everything() {
    let router = app(None).router();
    let mut open = Vec::new();
    for _ in 0..4 {
        let r = legacy_open(&router, "alice").await;
        assert_eq!(r.status(), StatusCode::OK);
        open.push(r);
    }
}

#[tokio::test]
async fn t07_legacy_sse_session_is_refused_and_its_permit_drops_with_it() {
    let cap = Cap::new(1);
    let app = app(None).with_session_admission(cap.clone());
    let router = app.router();

    let first = legacy_open(&router, "alice").await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(cap.live(), 1);
    assert_refused(legacy_open(&router, "alice").await).await;
    assert_eq!(cap.live(), 1, "refusal holds nothing");
    assert_eq!(
        cap.opens.lock().expect("opens")[0],
        (StreamKind::LegacySseSession, "LegacySse".to_string())
    );

    drop(first);
    eventually("legacy permit to drop", || cap.live() == 0).await;
    let again = legacy_open(&router, "alice").await;
    assert_eq!(again.status(), StatusCode::OK);
}

#[tokio::test]
async fn t07_streamable_session_is_refused_and_its_permit_drops_with_it() {
    let cap = Cap::new(1);
    let app = app(None).with_session_admission(cap.clone());
    let router = app.router();

    let r = streamable_initialize(&router, "alice").await;
    assert_eq!(r.status(), StatusCode::OK);
    let sid = sid_of(&r);
    assert_eq!(cap.live(), 1);
    assert_eq!(app.streamable_session_count(), 1);
    assert_eq!(
        cap.opens.lock().expect("opens")[0],
        (StreamKind::StreamableSession, "StreamableHttp".to_string())
    );

    assert_refused(streamable_initialize(&router, "alice").await).await;
    assert_eq!(cap.live(), 1);
    assert_eq!(app.streamable_session_count(), 1, "no session created");

    let r = send(
        &router,
        "DELETE",
        "/mcp",
        "alice",
        &session_headers(&sid),
        None,
    )
    .await;
    assert!(r.status().is_success(), "{}", r.status());
    eventually("session permit to drop", || cap.live() == 0).await;
    assert_eq!(app.streamable_session_count(), 0);
}

#[tokio::test]
async fn t07_session_get_stream_is_budgeted() {
    let cap = Cap::new(2);
    let router = app(None).with_session_admission(cap.clone()).router();
    let (sid, _) = streamable_session(&router, "alice").await;
    assert_eq!(cap.live(), 1, "the session");

    let stream = streamable_get(&router, &sid, "alice").await;
    assert_eq!(stream.status(), StatusCode::OK);
    assert_eq!(cap.live(), 2);
    assert_eq!(
        cap.opens.lock().expect("opens")[1],
        (StreamKind::ListenStream, "StreamableHttp".to_string())
    );
    assert_refused(streamable_get(&router, &sid, "alice").await).await;
    assert_eq!(cap.live(), 2);

    drop(stream);
    eventually("stream permit to drop", || cap.live() == 1).await;
}

#[tokio::test]
async fn t07_modern_listen_is_budgeted_and_stateless_calls_are_not() {
    let stub = Arc::new(Stub::default());
    let cap = Cap::new(1);
    let router = app(Some(stub)).with_session_admission(cap.clone()).router();

    // Terminal-first 2026-07-28 requests hold no stream, so no permit.
    let (mut body, mut headers) = modern_listen(&[]);
    body["method"] = json!("server/discover");
    body["params"] = json!({"_meta": body["params"]["_meta"].clone()});
    headers[2] = ("Mcp-Method", "server/discover");
    let r = send(&router, "POST", "/mcp", "alice", &headers, Some(&body)).await;
    assert_eq!(r.status(), StatusCode::OK);
    let _ = body_text(r).await;
    assert_eq!(cap.live(), 0);
    assert!(cap.opens.lock().expect("opens").is_empty());

    let first = listen(&router, "alice").await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(cap.live(), 1);
    assert_eq!(
        cap.opens.lock().expect("opens")[0],
        (StreamKind::ListenStream, "StreamableHttp".to_string())
    );
    assert_refused(listen(&router, "alice").await).await;

    drop(first);
    eventually("listen permit to drop", || cap.live() == 0).await;
    let again = listen(&router, "alice").await;
    assert_eq!(again.status(), StatusCode::OK);
}

#[tokio::test]
async fn t07_admission_is_consulted_per_principal() {
    // The stub only counts, but the principal reaches it: two principals, two opens.
    struct Seen(std::sync::Mutex<Vec<String>>);
    impl SessionAdmission for Seen {
        fn open(&self, req: StreamOpen<'_>) -> Result<StreamPermit, LimitExceeded> {
            let Principal::Federated { sub, .. } = req.principal else {
                panic!("federated principal expected");
            };
            self.0.lock().expect("seen").push(sub.clone());
            Ok(StreamPermit::unlimited())
        }
    }
    let seen = Arc::new(Seen(std::sync::Mutex::new(Vec::new())));
    let router = app(None).with_session_admission(seen.clone()).router();
    let _a = legacy_open(&router, "alice").await;
    let _b = legacy_open(&router, "bob").await;
    assert_eq!(*seen.0.lock().expect("seen"), ["alice", "bob"]);
}
