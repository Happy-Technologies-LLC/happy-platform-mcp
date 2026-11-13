//! C7 cancellation proof per protocol era (plan §4), through the real dispatcher
//! and `McpHandler`. The test tool is a remote mutation whose "ServiceNow call"
//! is one line over a loopback TCP connection to [`Stub`]; the stub answers only
//! when the test releases it, so a call can be held after its mutation was sent.
//!
//! - (a) `2026-07-28` HTTP: closing the request stream cancels; the outbound
//!   connection is closed promptly.
//! - (b) stdio and `2025-11-25` sessions: `notifications/cancelled` cancels.
//! - (c) legacy HTTP (`2025-11-25` session POST, `2024-11-05` SSE): a disconnect
//!   alone does not cancel; the call completes and is audited.
//! - (d) in every cancel case the mutation was sent exactly once and is never
//!   replayed; its audit attempt is never completed, so it stays `Unknown`.
//!
//! Also C11: a JSON-RPC batch from a `2025-03-26` client.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use hpm_protocol::McpHandler;
use hpm_protocol::transport::limits::HttpSecurity;
use hpm_protocol::transport::{HttpApp, HttpConfig};
use hpm_tools::dispatcher::{Dispatcher, RequiredPropertiesValidator, Services};
use hpm_tools::registration::ToolRegistry;
use hpm_tools::single_user::{AllowAll, LocalOnlyCredentials, PassThrough};
use hpm_types::BoxFut;
use hpm_types::audit::{
    AttemptRecord, AuditError, AuditEvent, AuditSink, PendingAttempt, SinkFailurePolicy,
    TerminalOutcome,
};
use hpm_types::context::{
    AdmissionKey, DeploymentMode, RequestContext, ResolvedInstance, TrustedRoot,
};
use hpm_types::credential::CredentialNeed;
use hpm_types::operation::{
    Action, ClassificationSnapshot, ClassificationSource, Operation, OperationSet,
    ScriptOperandGuard, Subject,
};
use hpm_types::tool::{
    AccessClass, HandlerCompletion, MutationOutcome, NormalizeInput, OpCompletion, OutputClass,
    ResultBody, RetryEligibility, SideEffectClass, ToolDescriptor, ToolError, ToolHandler,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const WRITE_TOOL: &str = "SN-Update-Record";
/// "Promptly": the outbound connection must close within this.
const PROMPT: Duration = Duration::from_secs(2);
/// How long a non-event (no close, no replay) is watched for.
const QUIET: Duration = Duration::from_millis(500);

static KEY: LazyLock<AdmissionKey> =
    LazyLock::new(|| AdmissionKey::mint().expect("first mint in this process"));

/// Loopback stand-in for ServiceNow. Counts mutation requests and closed
/// connections; answers only after [`Stub::release`].
#[derive(Default)]
struct Stub {
    sent: AtomicUsize,
    closed: AtomicUsize,
    changed: Notify,
    release: Notify,
}

impl Stub {
    async fn start() -> (Arc<Self>, SocketAddr) {
        let stub = Arc::new(Self::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
        let addr = listener.local_addr().expect("addr");
        let s = stub.clone();
        tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                let s = s.clone();
                tokio::spawn(async move { s.serve(conn).await });
            }
        });
        (stub, addr)
    }

    async fn serve(&self, conn: TcpStream) {
        let mut conn = BufReader::new(conn);
        let mut line = String::new();
        if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        self.sent.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
        let mut rest = [0u8; 64];
        tokio::select! {
            // EOF or reset: the caller dropped the connection.
            _ = conn.read(&mut rest) => {
                self.closed.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_waiters();
            }
            () = self.release.notified() => {
                let _ = conn.get_mut().write_all(b"ok\n").await;
            }
        }
    }

    /// Waits until `f` holds, up to `within`.
    async fn until(&self, within: Duration, f: impl Fn(&Self) -> bool) -> bool {
        tokio::time::timeout(within, async {
            loop {
                let changed = self.changed.notified();
                if f(self) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .is_ok()
    }

    async fn sent_once(&self) {
        assert!(
            self.until(Duration::from_secs(10), |s| s.sent.load(Ordering::SeqCst)
                == 1)
                .await,
            "mutation never reached the stub"
        );
    }

    async fn closed_promptly(&self) {
        assert!(
            self.until(PROMPT, |s| s.closed.load(Ordering::SeqCst) == 1)
                .await,
            "outbound call was not aborted"
        );
    }

    /// (d): one send, no replay.
    async fn never_replayed(&self) {
        tokio::time::sleep(QUIET).await;
        assert_eq!(self.sent.load(Ordering::SeqCst), 1, "mutation replayed");
    }
}

/// Sends one mutation line to the stub and waits for its answer, unless the
/// call's token is cancelled first (review M1: the test sees exactly what the
/// handler's own token sees). With `partial`, a first op committed before the
/// outbound one (review M2).
struct Outbound {
    stub: SocketAddr,
    partial: bool,
}
impl ToolHandler for Outbound {
    fn call<'a>(&'a self, ctx: &'a RequestContext, _: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move {
            let done = async {
                let mut conn = TcpStream::connect(self.stub).await?;
                conn.write_all(b"PATCH incident\n").await?;
                let mut reply = String::new();
                BufReader::new(conn).read_line(&mut reply).await?;
                Ok::<_, std::io::Error>(reply)
            };
            let reply = tokio::select! {
                r = done => r.ok(),
                () = ctx.cancel().cancelled() => None,
            };
            let mut ops = Vec::new();
            if self.partial {
                ops.push(OpCompletion {
                    op_index: 0,
                    outcome: MutationOutcome::Committed,
                });
            }
            let index = ops.len();
            if reply.as_deref() == Some("ok\n") {
                ops.push(OpCompletion {
                    op_index: index,
                    outcome: MutationOutcome::Committed,
                });
                return HandlerCompletion::new(
                    MutationOutcome::Committed,
                    ops,
                    Ok(ResultBody::Json(json!({"updated": true}))),
                );
            }
            let overall = if self.partial {
                MutationOutcome::PartiallyCommitted
            } else {
                MutationOutcome::Unknown
            };
            HandlerCompletion::new(overall, ops, Err(ToolError::Message("cancelled".into())))
        })
    }
}

const PARTIAL_TOOL: &str = "SN-Batch-Update";

fn normalize_two(_: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    let op = || {
        Operation::new(Subject::Tool(PARTIAL_TOOL), Action::Update)
            .on_table("incident", input.classification)
    };
    Ok(OperationSet(vec![op(), op()]))
}

fn normalize(_: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    Ok(OperationSet(vec![
        Operation::new(Subject::Tool(WRITE_TOOL), Action::Update)
            .on_table("incident", input.classification),
    ]))
}

fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::Json(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

struct KnownIncident;
impl ClassificationSource for KnownIncident {
    fn snapshot(&self, _: Option<&ResolvedInstance>) -> Arc<ClassificationSnapshot> {
        let mut s = ClassificationSnapshot::default();
        s.complete = true;
        s.known_tables.insert("incident".into());
        Arc::new(s)
    }
}

/// Counts attempts and their terminal outcomes.
#[derive(Default)]
struct Recorder {
    begun: AtomicUsize,
    completed: Mutex<Vec<MutationOutcome>>,
    changed: Notify,
}
impl AuditSink for Recorder {
    fn begin_attempt<'a>(
        &'a self,
        a: AttemptRecord,
    ) -> BoxFut<'a, Result<PendingAttempt, AuditError>> {
        self.begun.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(PendingAttempt { call_id: a.call_id }) })
    }
    fn complete<'a>(
        &'a self,
        _: &'a PendingAttempt,
        t: TerminalOutcome,
    ) -> BoxFut<'a, Result<(), AuditError>> {
        self.completed.lock().expect("lock").push(t.outcome);
        self.changed.notify_waiters();
        Box::pin(async { Ok(()) })
    }
    fn record<'a>(&'a self, _: AuditEvent) -> BoxFut<'a, Result<(), AuditError>> {
        Box::pin(async { Ok(()) })
    }
    fn failure_policy(&self) -> SinkFailurePolicy {
        SinkFailurePolicy::BestEffort
    }
    fn audits_reads(&self) -> bool {
        false
    }
}

impl Recorder {
    /// (d), review M2: the attempt is finalized, outcome unknown.
    async fn completed_unknown(&self) {
        self.completed_with(MutationOutcome::Unknown).await;
        assert_eq!(self.begun.load(Ordering::SeqCst), 1);
    }

    async fn completed_with(&self, outcome: MutationOutcome) {
        let ok = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let changed = self.changed.notified();
                if !self.completed.lock().expect("lock").is_empty() {
                    return;
                }
                changed.await;
            }
        })
        .await
        .is_ok();
        assert!(ok, "attempt never completed");
        assert_eq!(*self.completed.lock().expect("lock"), [outcome]);
    }
}

struct World {
    stub: Arc<Stub>,
    audit: Arc<Recorder>,
    dispatcher: Arc<Dispatcher>,
}

async fn world() -> World {
    let (stub, stub_addr) = Stub::start().await;
    let audit = Arc::new(Recorder::default());
    let mut registry = ToolRegistry::new().expect("registry");
    for (name, norm, partial) in [
        (WRITE_TOOL, normalize as hpm_types::tool::Normalizer, false),
        (PARTIAL_TOOL, normalize_two, true),
    ] {
        registry
            .register_tool(
                ToolDescriptor::new(
                    name,
                    "write",
                    r#"{"type":"object","properties":{}}"#,
                    norm,
                    Arc::new(Outbound {
                        stub: stub_addr,
                        partial,
                    }),
                    render,
                )
                .access(AccessClass::Write)
                .output(OutputClass::Records)
                .side_effect(SideEffectClass::Remote)
                .retry(RetryEligibility::BeforeSendOnly)
                .credentials(CredentialNeed::None),
            )
            .expect("register");
    }
    let services = Services {
        validator: Arc::new(RequiredPropertiesValidator),
        classification: Arc::new(KnownIncident),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: Arc::new(LocalOnlyCredentials),
        audit: audit.clone(),
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
        usage: None,
    };
    World {
        stub,
        audit,
        dispatcher: Arc::new(Dispatcher::new(registry, services, *KEY)),
    }
}

// ---------------------------------------------------------------- HTTP helpers

async fn http(w: &World) -> SocketAddr {
    let security = HttpSecurity::new(TOKEN, &[], &[]).expect("security");
    let config = HttpConfig::new(
        security,
        Arc::new(|| json!({"status": "healthy"})),
        Arc::new(|| Box::pin(async { Ok(json!([])) })),
    );
    let app = HttpApp::new(
        McpHandler::new(
            TrustedRoot::streamable_http(DeploymentMode::SingleUser),
            w.dispatcher.clone(),
        ),
        McpHandler::new(
            TrustedRoot::legacy_sse(DeploymentMode::SingleUser),
            w.dispatcher.clone(),
        ),
        config,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = app.serve(listener, std::future::pending()).await;
    });
    addr
}

/// Opens a connection and writes one request; the caller owns the response.
async fn open(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> TcpStream {
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n",
        addr.port()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    let mut s = TcpStream::connect(addr).await.expect("connect");
    s.write_all(req.as_bytes()).await.expect("write");
    s
}

/// Status, headers and (de-chunked when needed) body of a complete response.
async fn exchange(addr: SocketAddr, headers: &[(&str, &str)], body: &str) -> (u16, String, String) {
    let mut s = open(addr, "POST", "/mcp", headers, body).await;
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut raw))
        .await
        .expect("reply in time")
        .expect("read");
    let text = String::from_utf8_lossy(&raw).to_string();
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
    (status, head.to_string(), body)
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

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
    })
}

/// JSON-RPC payloads from a JSON or SSE body.
fn messages(body: &str) -> Vec<Value> {
    let sse: Vec<Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect();
    if sse.is_empty() {
        serde_json::from_str(body)
            .map(|v| vec![v])
            .unwrap_or_default()
    } else {
        sse
    }
}

const JSON_CT: (&str, &str) = ("Content-Type", "application/json");
const ACCEPT_BOTH: (&str, &str) = ("Accept", "application/json, text/event-stream");

fn call(id: u64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":WRITE_TOOL,"arguments":{}}})
}

/// A `version` Streamable session, initialized; returns its id.
async fn session(addr: SocketAddr, version: &str) -> String {
    let init = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
        "protocolVersion": version, "capabilities": {}, "clientInfo": {"name":"c7","version":"1"}}});
    let (status, head, body) = exchange(addr, &[JSON_CT, ACCEPT_BOTH], &init.to_string()).await;
    assert_eq!(status, 200, "{body}");
    let sid = header(&head, "mcp-session-id").expect("session id");
    let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    let h = session_headers(&sid, version);
    let (status, _, body) = exchange(addr, &h.each(), &note.to_string()).await;
    assert_eq!(status, 202, "{body}");
    sid
}

struct SessionHeaders(String, String);
impl SessionHeaders {
    fn each(&self) -> [(&str, &str); 4] {
        [
            JSON_CT,
            ACCEPT_BOTH,
            ("Mcp-Session-Id", self.0.as_str()),
            ("MCP-Protocol-Version", self.1.as_str()),
        ]
    }
}
fn session_headers(sid: &str, version: &str) -> SessionHeaders {
    SessionHeaders(sid.to_owned(), version.to_owned())
}

// ---------------------------------------------------------------------- tests

// (a) + (d): `2026-07-28` closing the request stream cancels the call.
#[tokio::test]
async fn modern_http_disconnect_cancels_and_never_replays() {
    let w = world().await;
    let addr = http(&w).await;
    let mut body = call(1);
    body["params"]["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "c7", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let conn = open(
        addr,
        "POST",
        "/mcp",
        &[
            JSON_CT,
            ACCEPT_BOTH,
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", WRITE_TOOL),
        ],
        &body.to_string(),
    )
    .await;
    w.stub.sent_once().await;
    drop(conn);
    w.stub.closed_promptly().await;
    w.stub.never_replayed().await;
    w.audit.completed_unknown().await;
}

// (b) + (d): `notifications/cancelled` on a `2025-11-25` Streamable session.
#[tokio::test]
async fn legacy_session_cancelled_notification_cancels() {
    const V: &str = "2025-11-25";
    let w = world().await;
    let addr = http(&w).await;
    let sid = session(addr, V).await;
    let h = session_headers(&sid, V);
    // The call's response stream stays open: only the notification may cancel.
    let _pending = open(addr, "POST", "/mcp", &h.each(), &call(5).to_string()).await;
    w.stub.sent_once().await;
    let cancel =
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}});
    let (status, _, body) = exchange(addr, &h.each(), &cancel.to_string()).await;
    assert_eq!(status, 202, "{body}");
    w.stub.closed_promptly().await;
    w.stub.never_replayed().await;
    w.audit.completed_unknown().await;
}

// (c): dropping a `2025-11-25` session POST stream alone does not cancel.
#[tokio::test]
async fn legacy_session_disconnect_does_not_cancel() {
    const V: &str = "2025-11-25";
    let w = world().await;
    let addr = http(&w).await;
    let sid = session(addr, V).await;
    let h = session_headers(&sid, V);
    let conn = open(addr, "POST", "/mcp", &h.each(), &call(6).to_string()).await;
    w.stub.sent_once().await;
    drop(conn);
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        w.stub.closed.load(Ordering::SeqCst),
        0,
        "disconnect cancelled the call"
    );
    w.stub.release.notify_one();
    w.audit.completed_with(MutationOutcome::Committed).await;
    assert_eq!(w.stub.sent.load(Ordering::SeqCst), 1);
}

/// An initialized `2024-11-05` HTTP+SSE session: its event stream and POST path.
async fn sse_session(addr: SocketAddr) -> (TcpStream, String) {
    let mut sse = open(addr, "GET", "/mcp", &[("Accept", "text/event-stream")], "").await;
    let mut buf = String::new();
    let mut chunk = [0u8; 4096];
    while !buf.contains("sessionId=") || !buf[buf.find("sessionId=").expect("id")..].contains('\n')
    {
        let n = tokio::time::timeout(Duration::from_secs(10), sse.read(&mut chunk))
            .await
            .expect("endpoint event")
            .expect("read");
        assert!(n > 0, "stream ended: {buf}");
        buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
    }
    let start = buf.find("sessionId=").expect("id") + "sessionId=".len();
    let sid = buf[start..]
        .split(['\r', '\n'])
        .next()
        .expect("id")
        .to_string();
    let path = format!("/mcp?sessionId={sid}");
    let init = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
        "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name":"c7","version":"1"}}});
    assert!(
        sse_post(addr, &path, init)
            .await
            .starts_with("HTTP/1.1 202")
    );
    while !buf.contains("\"id\":0") {
        let n = sse.read(&mut chunk).await.expect("read");
        assert!(n > 0);
        buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
    }
    let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    assert!(
        sse_post(addr, &path, note)
            .await
            .starts_with("HTTP/1.1 202")
    );
    (sse, path)
}

async fn sse_post(addr: SocketAddr, path: &str, msg: Value) -> String {
    let mut s = open(addr, "POST", path, &[JSON_CT], &msg.to_string()).await;
    let mut raw = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut raw)).await;
    String::from_utf8_lossy(&raw).to_string()
}

// (c): closing a `2024-11-05` HTTP+SSE stream alone does not cancel either.
#[tokio::test]
async fn legacy_sse_disconnect_does_not_cancel() {
    let w = world().await;
    let addr = http(&w).await;
    let (sse, path) = sse_session(addr).await;
    assert!(
        sse_post(addr, &path, call(7))
            .await
            .starts_with("HTTP/1.1 202")
    );
    w.stub.sent_once().await;
    drop(sse);
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        w.stub.closed.load(Ordering::SeqCst),
        0,
        "disconnect cancelled the call"
    );
    w.stub.release.notify_one();
    w.audit.completed_with(MutationOutcome::Committed).await;
    assert_eq!(w.stub.sent.load(Ordering::SeqCst), 1);
}

// (b) + (d), review F3: `notifications/cancelled` on a `2024-11-05` HTTP+SSE
// session reaches rmcp and stops the work, not just the late response.
#[tokio::test]
async fn legacy_sse_cancelled_notification_cancels() {
    let w = world().await;
    let addr = http(&w).await;
    let (_sse, path) = sse_session(addr).await;
    assert!(
        sse_post(addr, &path, call(7))
            .await
            .starts_with("HTTP/1.1 202")
    );
    w.stub.sent_once().await;
    let cancel =
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}});
    assert!(
        sse_post(addr, &path, cancel)
            .await
            .starts_with("HTTP/1.1 202")
    );
    w.stub.closed_promptly().await;
    w.stub.never_replayed().await;
    w.audit.completed_unknown().await;
}

/// An initialized stdio server; returns a sender and the reply lines.
async fn stdio(
    w: &World,
) -> (
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
) {
    let handler = McpHandler::new(TrustedRoot::single_user_stdio(), w.dispatcher.clone());
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (server_read, server_write) = tokio::io::split(server_io);
    tokio::spawn(hpm_protocol::stdio::serve_io(
        handler,
        server_read,
        server_write,
    ));
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut lines = BufReader::new(client_read).lines();
    send(&mut client_write, json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
        "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name":"c7","version":"1"}}}))
        .await;
    let init = next_line(&mut lines).await;
    assert!(init.contains("\"id\":0"), "{init}");
    send(
        &mut client_write,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    (client_write, lines)
}

async fn send(w: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, m: Value) {
    w.write_all(format!("{m}\n").as_bytes())
        .await
        .expect("write");
}

async fn next_line(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
) -> String {
    tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("reply in time")
        .expect("read")
        .expect("line")
}

// (b) + (d): stdio `notifications/cancelled`.
#[tokio::test]
async fn stdio_cancelled_notification_cancels() {
    let w = world().await;
    let (mut tx, _lines) = stdio(&w).await;
    send(&mut tx, call(5)).await;
    w.stub.sent_once().await;
    send(
        &mut tx,
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}),
    )
    .await;
    w.stub.closed_promptly().await;
    w.stub.never_replayed().await;
    w.audit.completed_unknown().await;
}

// Review M2: a cancel after a first op committed is finalized as
// PartiallyCommitted, keeping the committed-write record.
#[tokio::test]
async fn cancel_after_committed_op_is_partially_committed() {
    let w = world().await;
    let (mut tx, _lines) = stdio(&w).await;
    send(&mut tx, json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":PARTIAL_TOOL,"arguments":{}}}))
        .await;
    w.stub.sent_once().await;
    send(
        &mut tx,
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":8}}),
    )
    .await;
    w.stub.closed_promptly().await;
    w.audit
        .completed_with(MutationOutcome::PartiallyCommitted)
        .await;
}

// C11 (review L5): stdio rejects a batch with the same JSON-RPC error.
#[tokio::test]
async fn stdio_batch_rejected() {
    let w = world().await;
    let (mut tx, mut lines) = stdio(&w).await;
    send(&mut tx, json!([{"jsonrpc":"2.0","id":11,"method":"ping"}])).await;
    let reply: Value = serde_json::from_str(&next_line(&mut lines).await).expect("json");
    assert_eq!(reply["error"]["code"], -32600, "{reply}");
    assert_eq!(reply["id"], Value::Null, "{reply}");
    send(&mut tx, json!({"jsonrpc":"2.0","id":12,"method":"ping"})).await;
    let reply: Value = serde_json::from_str(&next_line(&mut lines).await).expect("json");
    assert_eq!(reply["id"], 12, "{reply}");
}

// C11 (review L5): legacy HTTP+SSE rejects a batch the same way.
#[tokio::test]
async fn sse_batch_rejected() {
    let w = world().await;
    let addr = http(&w).await;
    let (_sse, path) = sse_session(addr).await;
    let batch = json!([{"jsonrpc":"2.0","id":11,"method":"ping"}]);
    let raw = sse_post(addr, &path, batch).await;
    assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
    let body = raw.split_once("\r\n\r\n").expect("body").1;
    assert_eq!(
        serde_json::from_str::<Value>(body).expect("json"),
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Batch requests are not supported"}})
    );
}

// Owner decision (SEP-2164): an unknown resource URI is -32602 with `data.uri`.
#[tokio::test]
async fn unknown_resource_uri_is_invalid_params() {
    let w = world().await;
    let (mut tx, mut lines) = stdio(&w).await;
    send(&mut tx, json!({"jsonrpc":"2.0","id":9,"method":"resources/read","params":{"uri":"servicenow://nope"}}))
        .await;
    let reply: Value = serde_json::from_str(&next_line(&mut lines).await).expect("json");
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    assert_eq!(
        reply["error"]["data"],
        json!({"uri": "servicenow://nope"}),
        "{reply}"
    );
}

// C11: a `2025-03-26` client's JSON-RPC batch.
#[tokio::test]
async fn batch_from_2025_03_26_client() {
    const V: &str = "2025-03-26";
    let w = world().await;
    let addr = http(&w).await;
    let sid = session(addr, V).await;
    let h = session_headers(&sid, V);
    let batch = json!([
        {"jsonrpc":"2.0","id":11,"method":"tools/list"},
        {"jsonrpc":"2.0","id":12,"method":"ping"}
    ]);
    let (status, _, body) = exchange(addr, &h.each(), &batch.to_string()).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).expect("json"),
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Batch requests are not supported"}})
    );
    // The session is unaffected: a single request still works.
    let (status, _, body) = exchange(addr, &h.each(), &batch[1].to_string()).await;
    assert_eq!(status, 200, "{body}");
    assert!(messages(&body).iter().any(|m| m["id"] == 12), "{body}");
}
