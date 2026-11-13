//! End-to-end HTTP transport tests (ports of the server-level cases in 6.2.0
//! `tests/http-server.test.js`). They live in the composition root because only it
//! may build a `TrustedRoot`. Every server binds 127.0.0.1:0; no network beyond
//! loopback.

use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use hpm_protocol::McpHandler;
use hpm_protocol::transport::limits::{HttpLimits, HttpSecurity};
use hpm_protocol::transport::{HttpApp, HttpConfig, InstancesFn};
use hpm_tools::dispatcher::{Dispatcher, RequiredPropertiesValidator, Services};
use hpm_tools::registration::ToolRegistry;
use hpm_tools::single_user::{
    AllowAll, LocalOnlyCredentials, PassThrough, StderrAudit, UnloadedClassification,
};
use hpm_types::BoxFut;
use hpm_types::context::{AdmissionKey, DeploymentMode, RequestContext, TrustedRoot};
use hpm_types::credential::CredentialNeed;
use hpm_types::operation::{Action, Operation, OperationSet, ScriptOperandGuard, Subject};
use hpm_types::tool::{
    HandlerCompletion, NormalizeInput, OutputClass, ResultBody, ToolDescriptor, ToolError,
    ToolHandler,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SLOW_TOOL: &str = "SN-Query-Table";
const LEGACY: &str = "2024-11-05";

static KEY: LazyLock<AdmissionKey> =
    LazyLock::new(|| AdmissionKey::mint().expect("first mint in this process"));

/// Blocks until the test adds a permit, so a call can be held in flight.
struct Gate(Arc<Semaphore>);
impl ToolHandler for Gate {
    fn call<'a>(&'a self, _: &'a RequestContext, args: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move {
            if let Ok(p) = self.0.acquire().await {
                p.forget();
            }
            HandlerCompletion::not_a_mutation(Ok(ResultBody::Json(args)))
        })
    }
}

fn normalize(_: &Value, _: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    Ok(OperationSet(vec![Operation::new(
        Subject::Tool(SLOW_TOOL),
        Action::Read,
    )]))
}

fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::Json(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

fn dispatcher(gate: Arc<Semaphore>) -> Arc<Dispatcher> {
    let mut registry = ToolRegistry::new().expect("registry");
    registry
        .register_tool(
            ToolDescriptor::new(
                SLOW_TOOL,
                "held until released",
                r#"{"type":"object","properties":{}}"#,
                normalize,
                Arc::new(Gate(gate)),
                render,
            )
            .output(OutputClass::Records)
            .credentials(CredentialNeed::None),
        )
        .expect("register");
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
    Arc::new(Dispatcher::new(registry, services, *KEY))
}

struct Server {
    addr: SocketAddr,
    app: HttpApp,
    gate: Arc<Semaphore>,
    /// Resolves the `serve` shutdown future.
    stop: Arc<Notify>,
    serving: tokio::task::JoinHandle<()>,
}

async fn start(limits: HttpLimits) -> Server {
    start_with(limits, &[]).await
}

async fn start_with(limits: HttpLimits, origins: &[&str]) -> Server {
    start_full(limits, origins, Duration::from_secs(10)).await
}

/// An `/instances` provider answering `result` on every request.
fn listing(result: Result<Value, ()>) -> InstancesFn {
    Arc::new(move || {
        let result = result.clone();
        Box::pin(async move { result })
    })
}

async fn start_full(limits: HttpLimits, origins: &[&str], grace: Duration) -> Server {
    let instances = listing(Ok(json!([{"name": "dev", "default": true}])));
    start_app(limits, origins, grace, instances).await
}

async fn start_app(
    limits: HttpLimits,
    origins: &[&str],
    grace: Duration,
    instances: InstancesFn,
) -> Server {
    let gate = Arc::new(Semaphore::new(0));
    let d = dispatcher(gate.clone());
    let origins: Vec<String> = origins.iter().map(|s| s.to_string()).collect();
    let security = HttpSecurity::new(TOKEN, &[], &origins).expect("security");
    let mut config = HttpConfig::new(
        security,
        Arc::new(|| json!({"status": "healthy", "instance_name": "dev"})),
        instances,
    );
    config.limits = limits;
    config.keepalive = Duration::from_millis(100);
    config.shutdown_grace = grace;
    let app = HttpApp::new(
        McpHandler::new(
            TrustedRoot::streamable_http(DeploymentMode::SingleUser),
            d.clone(),
        ),
        McpHandler::new(TrustedRoot::legacy_sse(DeploymentMode::SingleUser), d),
        config,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let serving = app.clone();
    let stop = Arc::new(Notify::new());
    let stopped = stop.clone().notified_owned();
    let serving = tokio::spawn(async move {
        let _ = serving.serve(listener, stopped).await;
    });
    Server {
        addr,
        app,
        gate,
        stop,
        serving,
    }
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

fn parse(raw: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_string()
    };
    Reply {
        status,
        head: head.to_string(),
        body,
    }
}

fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if n == 0 || rest.len() < n {
            break;
        }
        out.push_str(&rest[..n]);
        s = rest[n..].trim_start_matches("\r\n");
    }
    out
}

fn request_bytes(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: 127.0.0.1:{}\r\n", addr.port()));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut s = TcpStream::connect(addr).await.expect("connect");
    s.write_all(&request_bytes(addr, method, path, headers, body))
        .await
        .expect("write");
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut raw))
        .await
        .expect("reply in time")
        .expect("read");
    parse(&raw)
}

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

async fn authed(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let auth = bearer();
    let mut h = vec![("Authorization", auth.as_str())];
    h.extend_from_slice(extra);
    send(addr, method, path, &h, body).await
}

async fn post_message(addr: SocketAddr, session: &str, msg: &Value) -> Reply {
    authed(
        addr,
        "POST",
        &format!("/mcp?sessionId={session}"),
        &[("Content-Type", "application/json")],
        msg.to_string().as_bytes(),
    )
    .await
}

/// An open legacy SSE stream.
struct SseStream {
    stream: TcpStream,
    buf: String,
}

impl SseStream {
    async fn open(addr: SocketAddr) -> Result<Self, Reply> {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let auth = bearer();
        let req = request_bytes(
            addr,
            "GET",
            "/mcp",
            &[
                ("Authorization", auth.as_str()),
                ("Accept", "text/event-stream"),
            ],
            b"",
        );
        stream.write_all(&req).await.expect("write");
        let mut s = Self {
            stream,
            buf: String::new(),
        };
        if !s.read_until("\r\n\r\n").await {
            return Err(parse(s.buf.as_bytes()));
        }
        if !s.buf.starts_with("HTTP/1.1 200") {
            let mut rest = Vec::new();
            let _ =
                tokio::time::timeout(Duration::from_secs(5), s.stream.read_to_end(&mut rest)).await;
            let mut raw = s.buf.into_bytes();
            raw.extend(rest);
            return Err(parse(&raw));
        }
        Ok(s)
    }

    /// Reads until `needle` appears; `false` on EOF or timeout.
    async fn read_until(&mut self, needle: &str) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut chunk = [0u8; 8192];
        while !self.buf.contains(needle) {
            match tokio::time::timeout_at(deadline, self.stream.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => self.buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
                _ => return false,
            }
        }
        true
    }

    async fn session_id(&mut self) -> String {
        assert!(self.read_until("event: endpoint").await);
        assert!(self.read_until("sessionId=").await);
        let start = self.buf.find("sessionId=").expect("id") + "sessionId=".len();
        while !self.buf[start..].contains('\n') {
            assert!(self.read_until("\n").await);
        }
        self.buf[start..]
            .split(['\r', '\n'])
            .next()
            .expect("id")
            .to_string()
    }

    /// Waits for the JSON-RPC message with `id` on the stream.
    async fn response(&mut self, id: &Value) -> Value {
        let needle = format!("\"id\":{id}");
        assert!(
            self.read_until(&needle).await,
            "no response for {id}: {}",
            self.buf
        );
        let line = self
            .buf
            .lines()
            .find(|l| l.starts_with("data: ") && l.contains(&needle))
            .expect("data line")
            .to_string();
        serde_json::from_str(&line["data: ".len()..]).expect("json")
    }

    /// `true` when the server ended the stream (EOF or chunked terminator).
    async fn ended(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut chunk = [0u8; 4096];
        loop {
            match tokio::time::timeout_at(deadline, self.stream.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(n)) => {
                    self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if self.buf.ends_with("0\r\n\r\n") {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    }
}

async fn initialized_session(addr: SocketAddr) -> (SseStream, String) {
    let mut sse = SseStream::open(addr).await.ok().expect("open");
    let sid = sse.session_id().await;
    let init = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
        "protocolVersion": LEGACY, "capabilities": {}, "clientInfo": {"name":"e2e","version":"1"}}});
    assert_eq!(post_message(addr, &sid, &init).await.status, 202);
    let r = sse.response(&json!(0)).await;
    assert_eq!(r["result"]["serverInfo"]["name"], "happy-platform-mcp");
    let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    assert_eq!(post_message(addr, &sid, &note).await.status, 202);
    (sse, sid)
}

// 'rejects unauthenticated requests before routing', 'rejects %s credentials'
#[tokio::test]
async fn front_guard_rejects_unauthenticated_and_foreign_requests() {
    let s = start_with(HttpLimits::default(), &["https://console.example.com"]).await;
    let r = send(s.addr, "GET", "/health", &[], b"").await;
    assert_eq!(r.status, 401);
    assert_eq!(r.header("www-authenticate").as_deref(), Some("Bearer"));
    assert_eq!(r.json(), json!({"error": "Unauthorized"}));
    let auth = bearer();
    let r = send(
        s.addr,
        "GET",
        "/health",
        &[("Authorization", &auth), ("Host", "evil.example")],
        b"",
    )
    .await;
    assert_eq!(
        (r.status, r.json()),
        (403, json!({"error": "Host not allowed"}))
    );
    let r = send(
        s.addr,
        "GET",
        "/health",
        &[("Authorization", &auth), ("Origin", "https://evil.example")],
        b"",
    )
    .await;
    assert_eq!(
        (r.status, r.json()),
        (403, json!({"error": "Origin not allowed"}))
    );
    let r = send(
        s.addr,
        "GET",
        "/health",
        &[
            ("Authorization", &auth),
            ("Origin", "https://console.example.com"),
        ],
        b"",
    )
    .await;
    assert_eq!(r.status, 200);
    // Host is checked before the bearer: a foreign Host never reaches token checks.
    let r = send(s.addr, "GET", "/health", &[("Host", "evil.example")], b"").await;
    assert_eq!(r.status, 403);
}

// 'rate-limits repeated authentication failures per peer'
#[tokio::test]
async fn repeated_auth_failures_are_rate_limited() {
    let limits = HttpLimits {
        auth_failure_limit: 2,
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let wrong = format!("Bearer {}", "f".repeat(64));
    for _ in 0..2 {
        let r = send(s.addr, "GET", "/health", &[("Authorization", &wrong)], b"").await;
        assert_eq!(r.status, 401);
    }
    let r = authed(s.addr, "GET", "/health", &[], b"").await;
    assert_eq!(r.status, 429, "even a valid token is refused while limited");
    assert_eq!(
        r.json(),
        json!({"error": "Too many failed authentication attempts"})
    );
    let retry: u64 = r
        .header("retry-after")
        .and_then(|v| v.parse().ok())
        .expect("retry-after");
    assert!((1..=60).contains(&retry));
}

// 'serves /health and /instances to authenticated callers'
#[tokio::test]
async fn health_and_instances() {
    let s = start(HttpLimits::default()).await;
    let r = authed(s.addr, "GET", "/health", &[], b"").await;
    assert_eq!(
        (r.status, r.json()["status"].clone()),
        (200, json!("healthy"))
    );
    let r = authed(s.addr, "GET", "/instances", &[], b"").await;
    assert_eq!(
        r.json(),
        json!({"instances": [{"name": "dev", "default": true}]})
    );
}

// 6.2.0 error boundary: a failed registry listing answers 500, never stale data.
#[tokio::test]
async fn instances_listing_failure_answers_500() {
    let s = start_app(
        HttpLimits::default(),
        &[],
        Duration::from_secs(10),
        listing(Err(())),
    )
    .await;
    let r = authed(s.addr, "GET", "/instances", &[], b"").await;
    assert_eq!(
        (r.status, r.json()),
        (500, json!({"error": "Internal server error"}))
    );
}

// 'opens an SSE session and serves MCP over it'
#[tokio::test]
async fn sse_session_round_trip() {
    let s = start(HttpLimits::default()).await;
    let (mut sse, sid) = initialized_session(s.addr).await;
    assert!(sse.buf.contains("cache-control: no-cache, no-transform"));
    let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    assert_eq!(post_message(s.addr, &sid, &list).await.status, 202);
    let r = sse.response(&json!(1)).await;
    assert_eq!(r["result"]["tools"][0]["name"], SLOW_TOOL);
    // Keepalive comments flow on idle streams.
    assert!(sse.read_until(": keepalive").await);
}

// 'refuses POSTs for unknown sessions', 'requires application/json', body errors
#[tokio::test]
async fn sse_post_errors() {
    let limits = HttpLimits {
        max_json_bytes: 256,
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let r = post_message(s.addr, "nope", &json!({"jsonrpc":"2.0","method":"x"})).await;
    // Recorded deviation: 404 (6.2.0 answered 400).
    assert_eq!(
        (r.status, r.json()),
        (404, json!({"error": "Invalid or missing session ID"}))
    );
    let (_sse, sid) = initialized_session(s.addr).await;
    let path = format!("/mcp?sessionId={sid}");
    let r = authed(
        s.addr,
        "POST",
        &path,
        &[("Content-Type", "text/plain")],
        b"{}",
    )
    .await;
    assert_eq!(r.status, 415);
    assert_eq!(
        r.json(),
        json!({"error": "Content-Type must be application/json"})
    );
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(1024));
    let r = authed(
        s.addr,
        "POST",
        &path,
        &[("Content-Type", "application/json")],
        big.as_bytes(),
    )
    .await;
    assert_eq!(
        (r.status, r.json()),
        (413, json!({"error": "Request body too large"}))
    );
    let r = authed(
        s.addr,
        "POST",
        &path,
        &[("Content-Type", "application/json")],
        b"{not json",
    )
    .await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({"error": "Invalid JSON body"}))
    );
}

// 'caps live SSE sessions'
#[tokio::test]
async fn caps_live_sessions() {
    let limits = HttpLimits {
        max_active_sessions: 1,
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let _first = SseStream::open(s.addr).await.ok().expect("first");
    let Err(r) = SseStream::open(s.addr).await else {
        panic!("second session admitted")
    };
    assert_eq!(r.status, 429);
    assert_eq!(r.header("retry-after").as_deref(), Some("1"));
    assert_eq!(r.json(), json!({"error": "Too many SSE sessions"}));
}

// 'closes sessions that never finish setup'
#[tokio::test]
async fn setup_timeout_closes_pending_session() {
    let limits = HttpLimits {
        session_setup_timeout: Duration::from_millis(200),
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let mut sse = SseStream::open(s.addr).await.ok().expect("open");
    let _ = sse.session_id().await;
    assert!(sse.ended().await);
    assert_eq!(s.app.sse_session_count(), 0);
}

// 'closes idle sessions'
#[tokio::test]
async fn idle_timeout_closes_session() {
    let limits = HttpLimits {
        session_idle_timeout: Duration::from_millis(300),
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let (mut sse, sid) = initialized_session(s.addr).await;
    assert!(sse.ended().await);
    assert_eq!(s.app.sse_session_count(), 0);
    let r = post_message(
        s.addr,
        &sid,
        &json!({"jsonrpc":"2.0","id":9,"method":"tools/list"}),
    )
    .await;
    assert_eq!(r.status, 404);
}

// 'releases a session when the client disconnects'
#[tokio::test]
async fn disconnect_releases_session() {
    let s = start(HttpLimits::default()).await;
    let (sse, _sid) = initialized_session(s.addr).await;
    assert_eq!(s.app.sse_session_count(), 1);
    drop(sse);
    for _ in 0..100 {
        if s.app.sse_session_count() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session not released after disconnect");
}

fn call(id: u64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":SLOW_TOOL,"arguments":{}}})
}

// 'rejects duplicate in-flight ids', 'caps outstanding requests per session'
#[tokio::test]
async fn outstanding_request_bookkeeping() {
    let limits = HttpLimits {
        max_outstanding_requests_per_session: 2,
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let (mut sse, sid) = initialized_session(s.addr).await;
    assert_eq!(post_message(s.addr, &sid, &call(1)).await.status, 202);
    let r = post_message(s.addr, &sid, &call(1)).await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({"error": "Duplicate request id"}))
    );
    assert_eq!(post_message(s.addr, &sid, &call(2)).await.status, 202);
    let r = post_message(s.addr, &sid, &call(3)).await;
    assert_eq!(
        (r.status, r.json()),
        (429, json!({"error": "Too many outstanding requests"}))
    );
    s.gate.add_permits(2);
    sse.response(&json!(1)).await;
    sse.response(&json!(2)).await;
    // Responses release their slots.
    assert_eq!(post_message(s.addr, &sid, &call(3)).await.status, 202);
    s.gate.add_permits(1);
    sse.response(&json!(3)).await;
}

// 'discards the late response of a cancelled request' (owner decision: no abort)
#[tokio::test]
async fn cancelled_request_response_is_discarded() {
    let s = start(HttpLimits::default()).await;
    let (mut sse, sid) = initialized_session(s.addr).await;
    assert_eq!(post_message(s.addr, &sid, &call(7)).await.status, 202);
    let cancel =
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}});
    let r = post_message(s.addr, &sid, &cancel).await;
    assert_eq!((r.status, r.body.as_str()), (202, "Accepted"));
    s.gate.add_permits(1);
    // A later request is answered while the cancelled one never is.
    let list = json!({"jsonrpc":"2.0","id":8,"method":"tools/list"});
    assert_eq!(post_message(s.addr, &sid, &list).await.status, 202);
    sse.response(&json!(8)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !sse.buf.contains("\"id\":7"),
        "cancelled response leaked: {}",
        sse.buf
    );
    // The cancelled id's slot was released when its response was dropped.
    assert_eq!(post_message(s.addr, &sid, &call(7)).await.status, 202);
}

// 'closeAllSessions ends live streams and refuses new sessions'
#[tokio::test]
async fn shutdown_closes_sessions_and_refuses_new_ones() {
    let s = start(HttpLimits::default()).await;
    let (mut sse, _sid) = initialized_session(s.addr).await;
    s.app.close_all_sessions();
    assert!(sse.ended().await);
    let Err(r) = SseStream::open(s.addr).await else {
        panic!("session admitted during shutdown")
    };
    assert_eq!(
        (r.status, r.json()),
        (503, json!({"error": "Server is shutting down"}))
    );
}

// Plan §5.3 rules 2 and 8 and the 405 for HEAD.
#[tokio::test]
async fn streamable_http_front_rules() {
    let s = start(HttpLimits::default()).await;
    let r = authed(s.addr, "HEAD", "/mcp", &[], b"").await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow").as_deref(), Some("GET, POST, DELETE"));
    let r = authed(
        s.addr,
        "POST",
        "/mcp",
        &[("Content-Type", "application/json")],
        b"{oops",
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], -32700);
    let body = json!({"jsonrpc":"2.0","id":4,"method":"tools/list"}).to_string();
    let r = authed(
        s.addr,
        "POST",
        "/mcp",
        &[
            ("Content-Type", "application/json"),
            ("MCP-Protocol-Version", "1999-01-01"),
        ],
        body.as_bytes(),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], -32022);
    assert_eq!(r.json()["error"]["data"]["requested"], "1999-01-01");
    assert!(
        r.json()["error"]["data"]["supported"]
            .as_array()
            .is_some_and(|v| v.contains(&json!("2026-07-28"))),
        "{}",
        r.body
    );
    assert_eq!(r.json()["id"], 4);
    let r = authed(
        s.addr,
        "POST",
        "/mcp?sessionId=a",
        &[
            ("Content-Type", "application/json"),
            ("Mcp-Session-Id", "b"),
        ],
        body.as_bytes(),
    )
    .await;
    assert_eq!(r.status, 400);
}

const ACCEPT_BOTH: (&str, &str) = ("Accept", "application/json, text/event-stream");
const JSON_CT: (&str, &str) = ("Content-Type", "application/json");

/// The JSON-RPC message with `id` from a JSON or SSE response body.
fn rpc_reply(r: &Reply, id: u64) -> Value {
    let needle = format!("\"id\":{id}");
    r.body
        .lines()
        .filter_map(|l| l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:")))
        .find(|l| l.contains(&needle))
        .and_then(|l| serde_json::from_str(l.trim()).ok())
        .unwrap_or_else(|| r.json())
}

fn assert_tool_round_trip(list: &Value, call: &Value) {
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, [SLOW_TOOL], "{list}");
    assert_ne!(call["result"]["isError"], json!(true), "{call}");
    assert!(call["result"]["content"].is_array(), "{call}");
}

async fn streamable_initialize(addr: SocketAddr, version: &str) -> Reply {
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion": version, "capabilities": {}, "clientInfo": {"name":"e2e","version":"1"}}});
    authed(
        addr,
        "POST",
        "/mcp",
        &[JSON_CT, ACCEPT_BOTH],
        init.to_string().as_bytes(),
    )
    .await
}

// Review H1: initialize + tools/list + tools/call over a `2025-11-25` session.
#[tokio::test]
async fn streamable_session_2025_11_25() {
    const V: &str = "2025-11-25";
    let s = start(HttpLimits::default()).await;
    s.gate.add_permits(1);
    let r = streamable_initialize(s.addr, V).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let init = rpc_reply(&r, 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "happy-platform-mcp");
    assert_eq!(init["result"]["protocolVersion"], V);
    let sid = r.header("mcp-session-id").expect("session id");
    let session = [
        JSON_CT,
        ACCEPT_BOTH,
        ("Mcp-Session-Id", sid.as_str()),
        ("MCP-Protocol-Version", V),
    ];
    let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    let r = authed(
        s.addr,
        "POST",
        "/mcp",
        &session,
        note.to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 202, "{}", r.body);
    let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    let r = authed(
        s.addr,
        "POST",
        "/mcp",
        &session,
        list.to_string().as_bytes(),
    )
    .await;
    let list = rpc_reply(&r, 2);
    let call = json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":SLOW_TOOL,"arguments":{}}});
    let r = authed(
        s.addr,
        "POST",
        "/mcp",
        &session,
        call.to_string().as_bytes(),
    )
    .await;
    assert_tool_round_trip(&list, &rpc_reply(&r, 3));
    assert_eq!(s.app.streamable_session_count(), 1);
}

fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "e2e", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

async fn modern(addr: SocketAddr, id: u64, method: &str, mut params: Value) -> Value {
    params["_meta"] = modern_meta();
    let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    let name = params["name"].as_str().map(str::to_string);
    let mut h = vec![
        JSON_CT,
        ACCEPT_BOTH,
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", method),
    ];
    if let Some(n) = &name {
        h.push(("Mcp-Name", n.as_str()));
    }
    let r = authed(addr, "POST", "/mcp", &h, body.to_string().as_bytes()).await;
    assert_eq!(r.status, 200, "{method}: {}", r.body);
    assert!(r.header("mcp-session-id").is_none(), "stateless");
    rpc_reply(&r, id)
}

// Review H1: the stateless `2026-07-28` path (server/discover, tools/list, tools/call).
#[tokio::test]
async fn streamable_stateless_2026_07_28() {
    let s = start(HttpLimits::default()).await;
    s.gate.add_permits(1);
    let discover = modern(s.addr, 1, "server/discover", json!({})).await;
    assert!(discover.get("result").is_some(), "{discover}");
    assert!(
        discover.to_string().contains("happy-platform-mcp"),
        "{discover}"
    );
    let list = modern(s.addr, 2, "tools/list", json!({})).await;
    let call = modern(
        s.addr,
        3,
        "tools/call",
        json!({"name": SLOW_TOOL, "arguments": {}}),
    )
    .await;
    assert_tool_round_trip(&list, &call);
    assert_eq!(s.app.streamable_session_count(), 0);
    // C9 / SEP-2575: `prompts` is not declared, so a modern prompts/list is an
    // unknown method rather than an empty list.
    let mut body = json!({"jsonrpc":"2.0","id":4,"method":"prompts/list","params":{}});
    body["params"]["_meta"] = modern_meta();
    let h = [
        JSON_CT,
        ACCEPT_BOTH,
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", "prompts/list"),
    ];
    let r = authed(s.addr, "POST", "/mcp", &h, body.to_string().as_bytes()).await;
    assert_eq!(rpc_reply(&r, 4)["error"]["code"], -32601, "{}", r.body);
}

// Review M2: Streamable sessions count against the shared live-session cap.
#[tokio::test]
async fn streamable_sessions_share_the_session_cap() {
    let limits = HttpLimits {
        max_active_sessions: 1,
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    assert_eq!(
        streamable_initialize(s.addr, "2025-11-25").await.status,
        200
    );
    let r = streamable_initialize(s.addr, "2025-11-25").await;
    assert_eq!(
        (r.status, r.json()),
        (429, json!({"error": "Too many sessions"}))
    );
    let Err(r) = SseStream::open(s.addr).await else {
        panic!("SSE session admitted over the shared cap")
    };
    assert_eq!(r.status, 429);
}

// Review F2: an initialize whose `_meta` names 2026-07-28 still opens an rmcp
// session, so it is counted against the cap and bound like any other.
#[tokio::test]
async fn modern_meta_initialize_is_counted_and_bound() {
    let limits = HttpLimits {
        max_active_sessions: 1,
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name":"e2e","version":"1"},
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}});
    let first = authed(
        s.addr,
        "POST",
        "/mcp",
        &[JSON_CT, ACCEPT_BOTH],
        init.to_string().as_bytes(),
    )
    .await;
    assert_eq!(first.status, 200);
    if first.header("mcp-session-id").is_some() {
        assert_eq!(s.app.streamable_session_count(), 1);
    }
    let r = streamable_initialize(s.addr, "2025-11-25").await;
    assert_eq!(
        (r.status, r.json()),
        (429, json!({"error": "Too many sessions"}))
    );
}

// Review F2: a session id the adapter never bound is refused before rmcp.
#[tokio::test]
async fn unbound_session_id_is_refused() {
    let s = start(HttpLimits::default()).await;
    let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
    let r = authed(
        s.addr,
        "POST",
        "/mcp",
        &[JSON_CT, ACCEPT_BOTH, ("Mcp-Session-Id", "never-bound")],
        list.to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json(), json!({"error": "Invalid or missing session ID"}));
}

// Review M3: messages rmcp would reject never reserve an outstanding slot.
#[tokio::test]
async fn invalid_messages_do_not_wedge_a_session() {
    let s = start(HttpLimits::default()).await;
    let (mut sse, sid) = initialized_session(s.addr).await;
    for id in 100..164 {
        let bad = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":"bad"});
        let r = post_message(s.addr, &sid, &bad).await;
        assert_eq!(
            (r.status, r.json()),
            (400, json!({"error": "Invalid message"}))
        );
    }
    let list = json!({"jsonrpc":"2.0","id":9,"method":"tools/list"});
    assert_eq!(post_message(s.addr, &sid, &list).await.status, 202);
    sse.response(&json!(9)).await;
}

// Review M4: a client that never reads its stream cannot hang shutdown.
#[tokio::test]
async fn shutdown_finishes_with_a_non_reading_client() {
    let s = start_full(HttpLimits::default(), &[], Duration::from_secs(1)).await;
    let (sse, sid) = initialized_session(s.addr).await;
    s.gate.add_permits(10_000);
    let pad = "x".repeat(256 * 1024);
    for id in 1..200u64 {
        let msg = json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":SLOW_TOOL,"arguments":{"pad":pad}}});
        if post_message(s.addr, &sid, &msg).await.status == 429 {
            break;
        }
    }
    s.stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), s.serving)
        .await
        .expect("serve returned despite a stalled stream")
        .expect("join");
    assert_eq!(s.app.sse_session_count(), 0);
    drop(sse);
}

// Review M1: incomplete headers are cut off before authentication runs.
#[tokio::test]
async fn slow_headers_are_timed_out() {
    let limits = HttpLimits {
        headers_timeout: Duration::from_millis(300),
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let mut c = TcpStream::connect(s.addr).await.expect("connect");
    c.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .await
        .expect("write");
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut raw))
        .await
        .expect("connection closed by the header timeout")
        .expect("read");
    let text = String::from_utf8_lossy(&raw);
    assert!(raw.is_empty() || text.starts_with("HTTP/1.1 408"), "{text}");
}

// Review M1: a body that never arrives is answered 408.
#[tokio::test]
async fn slow_body_is_timed_out() {
    let limits = HttpLimits {
        request_timeout: Duration::from_millis(300),
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let mut c = TcpStream::connect(s.addr).await.expect("connect");
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: {}\r\n\
         Content-Type: application/json\r\nConnection: close\r\nContent-Length: 100\r\n\r\n{{",
        s.addr.port(),
        bearer()
    );
    c.write_all(head.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut raw))
        .await
        .expect("reply in time")
        .expect("read");
    let r = parse(&raw);
    assert_eq!(
        (r.status, r.json()),
        (408, json!({"error": "Request timeout"}))
    );
}

/// Opens a keep-alive connection and authenticates it with one `/health` call.
async fn authed_keepalive(addr: SocketAddr) -> TcpStream {
    let mut c = TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: {}\r\n\r\n",
        addr.port(),
        bearer()
    );
    c.write_all(head.as_bytes()).await.expect("write");
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("reply in time")
        .expect("read");
    assert!(
        buf[..n].starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&buf[..n])
    );
    c
}

/// True when the server closes `c` without answering a request.
async fn refused(addr: SocketAddr, mut c: TcpStream) -> bool {
    let _ = c
        .write_all(&request_bytes(
            addr,
            "GET",
            "/health",
            &[("Authorization", &bearer())],
            b"",
        ))
        .await;
    let mut raw = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut raw))
        .await
        .expect("closed in time");
    read.is_err() || raw.is_empty()
}

// Review M1: once every slot holds an authenticated connection, further
// connections are closed at accept.
#[tokio::test]
async fn connection_cap_closes_excess_connections() {
    let limits = HttpLimits {
        max_connections: 2,
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let _a = authed_keepalive(s.addr).await;
    let _b = authed_keepalive(s.addr).await;
    let c = TcpStream::connect(s.addr).await.expect("c");
    assert!(refused(s.addr, c).await);
}

// Review F1: silent sockets from one address, filling the per-peer
// unauthenticated cap or the global cap, do not lock out an authenticated
// client from that address.
#[tokio::test]
async fn silent_sockets_do_not_lock_out_authenticated_client() {
    for limits in [
        HttpLimits {
            max_unauthenticated_per_peer: 4,
            ..HttpLimits::default()
        },
        HttpLimits {
            max_connections: 4,
            ..HttpLimits::default()
        },
    ] {
        let s = start(limits).await;
        let mut silent = Vec::new();
        for _ in 0..8 {
            silent.push(TcpStream::connect(s.addr).await.expect("silent"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let r = send(
            s.addr,
            "GET",
            "/health",
            &[("Authorization", &bearer())],
            b"",
        )
        .await;
        assert_eq!(r.status, 200);
    }
}

// Review F1: authenticated connections from one address (a reverse proxy's
// keep-alive pool) are not limited by the per-peer unauthenticated cap.
#[tokio::test]
async fn per_peer_cap_spares_authenticated_connections() {
    let limits = HttpLimits {
        max_unauthenticated_per_peer: 1,
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let mut pool = Vec::new();
    for _ in 0..3 {
        pool.push(authed_keepalive(s.addr).await);
    }
    let r = send(
        s.addr,
        "GET",
        "/health",
        &[("Authorization", &bearer())],
        b"",
    )
    .await;
    assert_eq!(r.status, 200);
    // Review L1: none of the pool was evicted to make room.
    for c in &mut pool {
        assert!(serves(s.addr, c).await, "pooled connection was evicted");
    }
}

/// True when `c`, kept alive, still answers an authenticated request.
async fn serves(addr: SocketAddr, c: &mut TcpStream) -> bool {
    let head = format!(
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: {}\r\n\r\n",
        addr.port(),
        bearer()
    );
    if c.write_all(head.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 1024];
    matches!(
        tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf)).await,
        Ok(Ok(n)) if buf[..n].starts_with(b"HTTP/1.1 200")
    )
}

// Review L1: one peer's silent connections over its cap evict its oldest,
// and only that one.
#[tokio::test]
async fn per_peer_cap_evicts_oldest_silent_connection() {
    const CAP: usize = 4;
    let limits = HttpLimits {
        max_unauthenticated_per_peer: CAP,
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let mut silent = Vec::new();
    for _ in 0..=CAP {
        silent.push(TcpStream::connect(s.addr).await.expect("silent"));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut buf = [0u8; 16];
    let first = tokio::time::timeout(Duration::from_secs(5), silent[0].read(&mut buf))
        .await
        .expect("oldest closed in time");
    assert!(matches!(first, Ok(0) | Err(_)), "oldest not evicted");
    let newest = tokio::time::timeout(Duration::from_millis(300), silent[CAP].read(&mut buf)).await;
    assert!(newest.is_err(), "newest connection was closed");
}

// Review F1: a connection that never authenticates is closed after
// `preauth_timeout`, well before the header timeout.
#[tokio::test]
async fn unauthenticated_connection_is_closed_after_preauth_timeout() {
    let limits = HttpLimits {
        preauth_timeout: Duration::from_millis(300),
        ..HttpLimits::default()
    };
    let s = start(limits).await;
    let mut c = TcpStream::connect(s.addr).await.expect("connect");
    let mut raw = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut raw))
        .await
        .expect("closed by the pre-auth deadline");
    assert!(read.is_err() || raw.is_empty());
}
