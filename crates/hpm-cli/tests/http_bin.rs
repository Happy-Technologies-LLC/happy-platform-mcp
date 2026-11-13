//! Runs the real `happy-platform-mcp-http` binary (the `src/server.js` port) on
//! 127.0.0.1:0 with an env-only configuration, then talks to it over loopback.
// Process and socket plumbing outside `#[test]` fns: failures should abort loudly.
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const REFUSED: &str = "Happy MCP Server HTTP startup refused: ";
const TIMEOUT: Duration = Duration::from_secs(20);
/// Distinctive fake credentials: no HTTP body may ever contain them.
const SN_USERNAME: &str = "svc-user-u5ern4me";
const SN_PASSWORD: &str = "pw-s3cr3t-9f2b7c1e";

/// Env-only configuration: temp HOME and registry path (no registry file, so the
/// `.env` fallback applies), fake ServiceNow credentials that are never used.
fn command(home: &Path, vars: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_happy-platform-mcp-http"));
    cmd.env_clear();
    for keep in ["PATH", "SystemRoot", "SYSTEMROOT", "TEMP", "TMP"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("HAPPY_CONFIG_PATH", home.join("instances.json"))
        .env("SERVICENOW_INSTANCE_URL", "https://dev.example.invalid")
        .env("SERVICENOW_USERNAME", SN_USERNAME)
        .env("SERVICENOW_PASSWORD", SN_PASSWORD)
        .env("PORT", "0")
        .env("HAPPY_MCP_API_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd
}

struct Server {
    child: Child,
    addr: SocketAddr,
    stdout: mpsc::Receiver<String>,
    stderr: Arc<Mutex<String>>,
    _home: tempfile::TempDir,
}

impl Server {
    /// Everything the server wrote to stderr so far (for failure messages).
    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Drained = (Arc<Mutex<String>>, std::thread::JoinHandle<()>);

/// Drains a child pipe on a thread so the child can never block on a full pipe.
/// The thread ends at EOF (child exit).
fn drain(pipe: impl Read + Send + 'static) -> Drained {
    let buf = Arc::new(Mutex::new(String::new()));
    let sink = buf.clone();
    let thread = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        let mut pipe = pipe;
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                return;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    });
    (buf, thread)
}

fn start(vars: &[(&str, &str)]) -> Server {
    let home = tempfile::tempdir().unwrap();
    let mut child = command(home.path(), vars).spawn().unwrap();
    let (stderr, _) = drain(child.stderr.take().unwrap());
    let (tx, rx) = mpsc::channel();
    let out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    let addr = loop {
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| {
                panic!(
                    "no listening line; stdout so far: {seen:?}; stderr: {}",
                    stderr.lock().unwrap()
                )
            });
        if let Some(rest) = line.strip_prefix("Happy MCP Server listening on http://") {
            let authority = rest.split(' ').next().unwrap();
            break authority.parse::<SocketAddr>().unwrap();
        }
        seen.push(line);
    };
    assert_eq!(
        seen,
        ["Default ServiceNow instance: default (https://dev.example.invalid)"]
    );
    Server {
        child,
        addr,
        stdout: rx,
        stderr,
        _home: home,
    }
}

/// Runs `cmd` to completion within [`TIMEOUT`], draining both pipes; a child still
/// running at the deadline is killed and the test fails (a refusal that turned
/// into a listening server must fail CI, not hang it). Returns (code, stdout, stderr).
fn run_bounded(mut cmd: Command) -> (i32, String, String) {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (stdout, out_thread) = drain(child.stdout.take().unwrap());
    let (stderr, err_thread) = drain(child.stderr.take().unwrap());
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "process still running after {TIMEOUT:?} (server started instead of refusing?); stdout: {}; stderr: {}",
                stdout.lock().unwrap(),
                stderr.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // The child has exited, so both pipes are at EOF and the threads finish.
    out_thread.join().unwrap();
    err_thread.join().unwrap();
    let out = stdout.lock().unwrap().clone();
    let err = stderr.lock().unwrap().clone();
    (status.code().unwrap_or(-1), out, err)
}

/// Runs the binary to completion (startup refusals).
fn refused(vars: &[(&str, &str)]) -> (i32, String) {
    let home = tempfile::tempdir().unwrap();
    let (code, _, err) = run_bounded(command(home.path(), vars));
    (code, err)
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
    /// The JSON-RPC message with `id` from a JSON or SSE body.
    fn rpc(&self, id: u64) -> Value {
        let needle = format!("\"id\":{id}");
        self.body
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .find(|l| l.contains(&needle))
            .and_then(|l| serde_json::from_str(l.trim()).ok())
            .unwrap_or_else(|| self.json())
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
    body: &str,
) -> Vec<u8> {
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: 127.0.0.1:{}\r\n", addr.port()));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    req.into_bytes()
}

fn send(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(TIMEOUT)).unwrap();
    s.write_all(&request_bytes(addr, method, path, headers, body))
        .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
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

const BEARER: &str = "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const JSON_CT: (&str, &str) = ("Content-Type", "application/json");
const ACCEPT_BOTH: (&str, &str) = ("Accept", "application/json, text/event-stream");

fn authed(addr: SocketAddr, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> Reply {
    let mut h = vec![("Authorization", BEARER)];
    h.extend_from_slice(extra);
    send(addr, method, path, &h, body)
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result: {list}"))
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// Lanes register tools over time: assert shape, not a fixed list. Every listed
/// name is a 6.2.0 tool, listed once, in 6.2.0 `tools/list` order.
fn assert_known_tools(names: &[String]) {
    let ranks: Vec<usize> = names
        .iter()
        .map(|n| hpm_tools::tool_order::rank(n).unwrap_or_else(|| panic!("unknown tool {n}")))
        .collect();
    assert!(ranks.windows(2).all(|w| w[0] < w[1]), "{names:?}");
}

#[test]
fn guard_rejects_missing_token_and_foreign_host_or_origin() {
    let s = start(&[("HAPPY_MCP_ALLOWED_ORIGINS", "https://console.example.com")]);
    let r = send(s.addr, "GET", "/health", &[], "");
    assert_eq!(r.status, 401);
    assert_eq!(r.header("www-authenticate").as_deref(), Some("Bearer"));
    assert_eq!(r.json(), json!({"error": "Unauthorized"}));
    let r = send(s.addr, "POST", "/mcp", &[JSON_CT, ACCEPT_BOTH], "{}");
    assert_eq!(
        (r.status, r.json()),
        (401, json!({"error": "Unauthorized"})),
        "stderr: {}",
        s.stderr()
    );
    // SSE open and the instance listing sit behind the same guard.
    for (path, accept) in [
        ("/mcp", ("Accept", "text/event-stream")),
        ("/instances", ("Accept", "application/json")),
    ] {
        let r = send(s.addr, "GET", path, &[accept], "");
        assert_eq!(
            (r.status, r.json()),
            (401, json!({"error": "Unauthorized"})),
            "GET {path}; stderr: {}",
            s.stderr()
        );
        assert_eq!(r.header("www-authenticate").as_deref(), Some("Bearer"));
    }

    let r = authed(s.addr, "GET", "/health", &[("Host", "evil.example")], "");
    assert_eq!(
        (r.status, r.json()),
        (403, json!({"error": "Host not allowed"}))
    );
    let r = authed(
        s.addr,
        "GET",
        "/health",
        &[("Origin", "https://evil.example")],
        "",
    );
    assert_eq!(
        (r.status, r.json()),
        (403, json!({"error": "Origin not allowed"}))
    );
    let r = authed(
        s.addr,
        "GET",
        "/health",
        &[("Origin", "https://console.example.com")],
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
}

#[test]
fn health_and_instances_report_the_default_instance() {
    let s = start(&[]);
    let r = authed(s.addr, "GET", "/health", &[], "");
    assert_eq!(r.status, 200, "{}; stderr: {}", r.body, s.stderr());
    let h = r.json();
    let timestamp = h["timestamp"].as_str().unwrap_or_default().to_string();
    assert!(timestamp.ends_with('Z'), "{h}");
    // Exactly the 6.2.0 field set: nothing else (no credentials) may appear.
    assert_eq!(
        h,
        json!({
            "status": "healthy",
            "servicenow_instance": "https://dev.example.invalid",
            "instance_name": "default",
            "timestamp": timestamp,
        })
    );
    let health_body = r.body;
    let r = authed(s.addr, "GET", "/instances", &[], "");
    assert_eq!(r.status, 200, "{}; stderr: {}", r.body, s.stderr());
    assert_eq!(
        r.json(),
        json!({"instances": [{
            "name": "default",
            "url": "https://dev.example.invalid",
            "default": true,
            "description": "Loaded from .env",
        }]})
    );
    for body in [&health_body, &r.body] {
        for secret in [SN_PASSWORD, SN_USERNAME, TOKEN] {
            assert!(!body.contains(secret), "{secret} leaked in {body}");
        }
    }
}

#[test]
fn streamable_http_initialize_and_tools_list() {
    const V: &str = "2025-11-25";
    let s = start(&[]);
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion": V, "capabilities": {}, "clientInfo": {"name":"bin","version":"1"}}});
    let r = authed(
        s.addr,
        "POST",
        "/mcp",
        &[JSON_CT, ACCEPT_BOTH],
        &init.to_string(),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let init = r.rpc(1);
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
    let r = authed(s.addr, "POST", "/mcp", &session, &note.to_string());
    assert_eq!(r.status, 202, "{}", r.body);
    let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    let r = authed(s.addr, "POST", "/mcp", &session, &list.to_string());
    assert_eq!(r.status, 200, "{}", r.body);
    assert_known_tools(&tool_names(&r.rpc(2)));
}

/// An open legacy SSE stream.
struct Sse {
    stream: TcpStream,
    buf: String,
}

impl Sse {
    fn open(addr: SocketAddr) -> Self {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream
            .write_all(&request_bytes(
                addr,
                "GET",
                "/mcp",
                &[("Authorization", BEARER), ("Accept", "text/event-stream")],
                "",
            ))
            .unwrap();
        let mut s = Self {
            stream,
            buf: String::new(),
        };
        s.read_until("\r\n\r\n");
        assert!(s.buf.starts_with("HTTP/1.1 200"), "{}", s.buf);
        s
    }

    fn read_more(&mut self) {
        let mut chunk = [0u8; 8192];
        let n = self.stream.read(&mut chunk).unwrap();
        assert!(n > 0, "stream ended: {}", self.buf);
        self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
    }

    fn read_until(&mut self, needle: &str) {
        while !self.buf.contains(needle) {
            self.read_more();
        }
    }

    /// The rest of the first complete line containing `needle`, from `needle` on.
    fn line_from(&mut self, needle: &str) -> String {
        loop {
            if let Some(at) = self.buf.find(needle)
                && let Some(end) = self.buf[at..].find('\n')
            {
                return self.buf[at..at + end].trim_end_matches('\r').to_string();
            }
            self.read_more();
        }
    }

    fn session_id(&mut self) -> String {
        self.line_from("sessionId=")["sessionId=".len()..].to_string()
    }

    fn response(&mut self, id: u64) -> Value {
        let needle = format!("\"id\":{id}");
        self.line_from(&needle);
        let line = self
            .buf
            .lines()
            .find(|l| l.starts_with("data: ") && l.contains(&needle))
            .unwrap();
        serde_json::from_str(&line["data: ".len()..]).unwrap()
    }
}

#[test]
fn legacy_sse_initialize_and_tools_list() {
    let s = start(&[]);
    let mut sse = Sse::open(s.addr);
    let sid = sse.session_id();
    let path = format!("/mcp?sessionId={sid}");
    let post = |msg: Value| authed(s.addr, "POST", &path, &[JSON_CT], &msg.to_string());
    let init = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
        "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name":"bin","version":"1"}}});
    assert_eq!(post(init).status, 202);
    assert_eq!(
        sse.response(0)["result"]["serverInfo"]["name"],
        "happy-platform-mcp"
    );
    assert_eq!(
        post(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).status,
        202
    );
    assert_eq!(
        post(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).status,
        202
    );
    assert_known_tools(&tool_names(&sse.response(1)));
}

#[test]
fn debug_prints_the_active_instance() {
    let s = start(&[("DEBUG", "true")]);
    let deadline = Instant::now() + TIMEOUT;
    let mut lines = Vec::new();
    while !lines
        .iter()
        .any(|l: &String| l.starts_with("Active ServiceNow"))
    {
        let wait = deadline.saturating_duration_since(Instant::now());
        lines.push(s.stdout.recv_timeout(wait).unwrap());
    }
    let port = s.addr.port();
    assert_eq!(
        lines,
        [
            format!("Health check: http://127.0.0.1:{port}/health"),
            format!("MCP SSE endpoint: http://127.0.0.1:{port}/mcp"),
            format!("Available instances: http://127.0.0.1:{port}/instances"),
            hpm_cli::updates_hint(),
            "Active ServiceNow instance: default - https://dev.example.invalid".to_string(),
        ]
    );
}

#[cfg(unix)]
#[test]
fn sigterm_closes_sessions_and_exits_zero() {
    let mut s = start(&[]);
    let mut sse = Sse::open(s.addr);
    let _ = sse.session_id();
    send_signal(&s, "-TERM");
    let deadline = Instant::now() + TIMEOUT;
    wait_for_line(&s, deadline, "Received SIGTERM; closing MCP sessions");
    let exit = wait_exit(&mut s, deadline);
    assert_eq!(exit.code(), Some(0), "stderr: {}", s.stderr());
}

#[cfg(unix)]
fn send_signal(s: &Server, sig: &str) {
    let status = Command::new("kill")
        .args([sig, &s.child.id().to_string()])
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(unix)]
fn wait_for_line(s: &Server, deadline: Instant, want: &str) {
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        let line = s
            .stdout
            .recv_timeout(wait)
            .unwrap_or_else(|_| panic!("no {want:?}; stderr: {}", s.stderr()));
        if line == want {
            return;
        }
    }
}

#[cfg(unix)]
fn wait_exit(s: &mut Server, deadline: Instant) -> std::process::ExitStatus {
    loop {
        if let Some(code) = s.child.try_wait().unwrap() {
            return code;
        }
        assert!(
            Instant::now() < deadline,
            "server did not exit; stderr: {}",
            s.stderr()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A request whose body never finishes holds the graceful shutdown for the full
/// grace period; a second signal must cut it short.
#[cfg(unix)]
#[test]
fn second_signal_forces_immediate_exit() {
    let mut s = start(&[]);
    let mut stalled = TcpStream::connect(s.addr).unwrap();
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: {BEARER}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: 1000\r\n\r\n{{",
        s.addr.port()
    );
    stalled.write_all(head.as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    send_signal(&s, "-TERM");
    let deadline = Instant::now() + TIMEOUT;
    wait_for_line(&s, deadline, "Received SIGTERM; closing MCP sessions");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        s.child.try_wait().unwrap().is_none(),
        "server exited before the second signal; the stalled request did not hold shutdown"
    );
    let second = Instant::now();
    send_signal(&s, "-INT");
    let exit = wait_exit(&mut s, second + Duration::from_secs(3));
    assert_eq!(exit.code(), Some(130), "stderr: {}", s.stderr());
    drop(stalled);
}

#[test]
fn startup_refuses_without_a_valid_token() {
    let required = format!(
        "{REFUSED}HAPPY_MCP_API_TOKEN is required for the HTTP transport; generate one with `openssl rand -hex 32` and set it in the process environment (.env files are not read)\n"
    );
    assert_eq!(refused(&[("HAPPY_MCP_API_TOKEN", "")]), (1, required));
    let malformed = format!(
        "{REFUSED}HAPPY_MCP_API_TOKEN must be 32 random bytes encoded as 64 hex or 43 base64url characters; generate one with `openssl rand -hex 32` and set it in the process environment (.env files are not read)\n"
    );
    assert_eq!(refused(&[("HAPPY_MCP_API_TOKEN", "short")]), (1, malformed));
}

#[test]
fn startup_refuses_invalid_settings() {
    let (code, err) = refused(&[("HAPPY_MCP_ALLOWED_HOSTS", "https://x.example/")]);
    assert_eq!(code, 1);
    assert!(
        err.starts_with(&format!(
            "{REFUSED}HAPPY_MCP_ALLOWED_HOSTS: Invalid allowed host entry"
        )),
        "{err}"
    );
    let (code, err) = refused(&[("HAPPY_MCP_ALLOWED_ORIGINS", "*")]);
    assert_eq!(code, 1);
    assert!(
        err.starts_with(&format!(
            "{REFUSED}HAPPY_MCP_ALLOWED_ORIGINS: Invalid allowed origin entry"
        )),
        "{err}"
    );
    assert_eq!(
        refused(&[("SSE_KEEPALIVE_INTERVAL", "0")]),
        (
            1,
            format!("{REFUSED}keepaliveIntervalMs must be a positive integer\n")
        )
    );
    assert_eq!(
        refused(&[("PORT", "70000")]),
        (
            1,
            format!("{REFUSED}PORT must be an integer between 0 and 65535\n")
        )
    );
}

#[test]
fn main_binary_rejects_unknown_flags_with_exit_2() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_happy-platform-mcp"));
    cmd.arg("--http");
    let (code, _, err) = run_bounded(cmd);
    assert_eq!(code, 2);
    assert_eq!(err, "Unknown command. Use --help for usage.\n");
}
