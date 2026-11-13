//! Single-user HTTP: one axum app, `/mcp` dispatched per plan §5.3 rules 1–8.
//!
//! - rmcp `StreamableHttpService` (legacy_session_mode = true) serves both eras:
//!   `2026-07-28` stateless and `2025-11-25`/`2025-06-18`/`2025-03-26` sessions.
//! - A hand-written legacy HTTP+SSE adapter (2024-11-05 transport) shares `/mcp`.

use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{any, get},
};
use futures::StreamExt;
use rmcp::{
    ServiceExt,
    model::ProtocolVersion,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_stream::wrappers::ReceiverStream;

use crate::server::{HpmServer, event};

pub const MAX_JSON_BYTES: usize = 8 * 1024 * 1024; // 6.2.0 `maxJsonBytes`
const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const META_VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const MODERN: &str = "2026-07-28";

pub struct Security {
    pub token: String,
    pub port: u16,
    pub extra_hosts: Vec<String>,
    pub origins: Vec<String>,
}

type SseWriter = Arc<tokio::sync::Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>>;

struct AppState {
    sec: Security,
    rmcp: StreamableHttpService<HpmServer, LocalSessionManager>,
    sse: Mutex<HashMap<String, SseWriter>>,
    server: HpmServer,
}

pub fn router(server: HpmServer, sec: Security) -> Router {
    let hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"]
        .iter()
        .map(|h| h.to_string())
        .chain(sec.extra_hosts.iter().cloned())
        .collect();
    // F7: an empty rmcp Origin allowlist disables Origin checks; enforce explicitly.
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(true)
        .with_allowed_hosts(hosts)
        .with_allowed_origins(sec.origins.clone())
        .enforce_origin_validation()
        .with_max_request_body_bytes(MAX_JSON_BYTES)
        .with_sse_keep_alive(Some(Duration::from_secs(15)));
    let factory_server = server.clone();
    let rmcp = StreamableHttpService::new(
        move || Ok(factory_server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let state = Arc::new(AppState { sec, rmcp, sse: Mutex::new(HashMap::new()), server });
    Router::new()
        .route("/mcp", any(mcp))
        .route("/health", get(health))
        .route("/instances", get(instances))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn jsonrpc_error(status: StatusCode, id: Value, code: i64, msg: &str) -> Response {
    (status, Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })))
        .into_response()
}

// ---------------------------------------------------------------- rule 1

fn host_allowed(sec: &Security, headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let loopback = ["localhost", "127.0.0.1", "[::1]"];
    loopback.iter().any(|h| host == *h || host == format!("{h}:{}", sec.port))
        || sec.extra_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host))
}

fn origin_allowed(sec: &Security, headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN) {
        None => true,
        Some(o) => o.to_str().is_ok_and(|o| sec.origins.iter().any(|a| a.eq_ignore_ascii_case(o))),
    }
}

fn bearer_ok(sec: &Security, headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some((scheme, token)) = value.split_once(' ') else { return false };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    let (a, b) = (token.as_bytes(), sec.token.as_bytes());
    // Constant-time over the expected length.
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let h = req.headers();
    if !host_allowed(&st.sec, h) {
        return json_error(StatusCode::FORBIDDEN, "Host not allowed");
    }
    if !origin_allowed(&st.sec, h) {
        return json_error(StatusCode::FORBIDDEN, "Origin not allowed");
    }
    if !bearer_ok(&st.sec, h) {
        let mut r = json_error(StatusCode::UNAUTHORIZED, "Unauthorized");
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return r;
    }
    next.run(req).await
}

// ---------------------------------------------------------------- /mcp dispatch

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
        .any(|m| m.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("text/event-stream"))
}

fn has_modern_meta(body: &Value) -> bool {
    body.get("params")
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get(META_VERSION_KEY))
        .and_then(Value::as_str)
        .is_some_and(|v| v >= MODERN)
}

fn is_supported_version(v: &str) -> bool {
    ProtocolVersion::KNOWN_VERSIONS.iter().any(|k| k.as_str() == v)
}

async fn forward(st: &AppState, parts: axum::http::request::Parts, body: Body) -> Response {
    st.rmcp.handle(Request::from_parts(parts, body)).await.map(Body::new)
}

async fn mcp(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let query_sid = query_session_id(&req);
    let header_sid = req.headers().contains_key(SESSION_HEADER);
    // Rule 2.
    if query_sid.is_some() && header_sid {
        return json_error(StatusCode::BAD_REQUEST, "Both sessionId query and Mcp-Session-Id header supplied");
    }
    let (parts, body) = req.into_parts();
    match parts.method {
        Method::POST => {
            let bytes = match axum::body::to_bytes(body, MAX_JSON_BYTES).await {
                Ok(b) => b,
                Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"),
            };
            // Rule 8: malformed JSON-RPC → 400 / -32700 before any routing.
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                return jsonrpc_error(StatusCode::BAD_REQUEST, Value::Null, -32700, "Parse error");
            };
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            // Rule 8: unsupported protocol version → 400 / -32022 (rmcp answers plain text).
            if let Some(v) = parts.headers.get(VERSION_HEADER) {
                match v.to_str() {
                    Ok(v) if is_supported_version(v) => {}
                    _ => {
                        return jsonrpc_error(StatusCode::BAD_REQUEST, id, -32022, "Unsupported protocol version");
                    }
                }
            }
            // Rule 3: modern per-request `_meta` → rmcp stateless; a stale session id never downgrades.
            if has_modern_meta(&value) {
                let mut parts = parts;
                parts.headers.remove(SESSION_HEADER);
                return forward(&st, parts, Body::from(bytes)).await;
            }
            // Rule 7: legacy HTTP+SSE POST.
            if let Some(sid) = query_sid {
                return sse_post(&st, &sid, &value).await;
            }
            // Rules 4, 5 and the remainder of 8 (rmcp answers sessionless non-initialize with 400).
            forward(&st, parts, Body::from(bytes)).await
        }
        Method::GET => {
            if header_sid {
                return forward(&st, parts, body).await; // Rule 5.
            }
            if accepts_sse(&parts.headers) {
                return sse_open(st.clone()).await; // Rule 6.
            }
            json_error(StatusCode::NOT_ACCEPTABLE, "GET /mcp requires Accept: text/event-stream")
        }
        Method::DELETE if header_sid => forward(&st, parts, body).await, // Rule 5.
        _ => {
            let mut r = json_error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
            r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("GET, POST, DELETE"));
            r
        }
    }
}

// ---------------------------------------------------------------- legacy HTTP+SSE adapter

/// Removes the session when the SSE response stream is dropped (client disconnect).
struct SseSessionGuard {
    st: Arc<AppState>,
    sid: String,
}
impl Drop for SseSessionGuard {
    fn drop(&mut self) {
        self.st.sse.lock().expect("lock").remove(&self.sid);
        event(format!("sse session {} closed (stream dropped)", self.sid));
    }
}

async fn sse_open(st: Arc<AppState>) -> Response {
    let sid = uuid::Uuid::new_v4().to_string();
    let (server_end, adapter_end) = tokio::io::duplex(1 << 20);
    let (adapter_r, adapter_w) = tokio::io::split(adapter_end);
    let (server_r, server_w) = tokio::io::split(server_end);
    let handler = st.server.clone();
    let sid_task = sid.clone();
    tokio::spawn(async move {
        match handler.serve((server_r, server_w)).await {
            Ok(running) => {
                let reason = running.waiting().await;
                event(format!("sse session {sid_task} service ended: {reason:?}"));
            }
            Err(e) => event(format!("sse session {sid_task} init failed: {e}")),
        }
    });
    st.sse.lock().expect("lock").insert(sid.clone(), Arc::new(tokio::sync::Mutex::new(adapter_w)));
    event(format!("sse session {sid} opened"));

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    let endpoint = Event::default().event("endpoint").data(format!("/mcp?sessionId={sid}"));
    let _ = tx.send(Ok(endpoint)).await;
    tokio::spawn(async move {
        let mut lines = BufReader::new(adapter_r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(Ok(Event::default().event("message").data(line))).await.is_err() {
                break;
            }
        }
    });
    let guard = SseSessionGuard { st, sid };
    let stream = ReceiverStream::new(rx).map(move |e| {
        let _keep = &guard;
        e
    });
    let mut r = Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
    r.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}

async fn sse_post(st: &AppState, sid: &str, value: &Value) -> Response {
    let writer = st.sse.lock().expect("lock").get(sid).cloned();
    let Some(writer) = writer else {
        return json_error(StatusCode::NOT_FOUND, "Unknown or expired session");
    };
    let mut line = serde_json::to_vec(value).expect("re-serializable");
    line.push(b'\n');
    if writer.lock().await.write_all(&line).await.is_err() {
        return json_error(StatusCode::NOT_FOUND, "Unknown or expired session");
    }
    (StatusCode::ACCEPTED, "Accepted").into_response()
}

// ---------------------------------------------------------------- other routes (6.2.0 shape)

async fn health(State(st): State<Arc<AppState>>) -> Response {
    let (url, name) = st.server.instance_label();
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    Json(json!({
        "status": "healthy",
        "servicenow_instance": url,
        "instance_name": name,
        "timestamp_unix_ms": ts.as_millis() as u64,
    }))
    .into_response()
}

async fn instances(State(st): State<Arc<AppState>>) -> Response {
    let (url, name) = st.server.instance_label();
    Json(json!({ "instances": [{ "name": name, "url": url, "default": true, "description": "" }] })).into_response()
}
