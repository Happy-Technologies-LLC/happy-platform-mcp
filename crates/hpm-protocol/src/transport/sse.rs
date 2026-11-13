//! Legacy HTTP+SSE adapter (MCP `2024-11-05` transport) on `/mcp`, porting the
//! session limits of 6.2.0 `src/http-server.js` `createHttpApp`.
//!
//! - `GET /mcp` opens a session; the first event is `endpoint` = `/mcp?sessionId=<id>`.
//! - `POST /mcp?sessionId=<id>` forwards one JSON-RPC message and answers `202`.
//! - Pending sessions (opened, not initialized) and live sessions are capped; idle,
//!   lifetime and setup timers close sessions; per-session and global POST queues
//!   and outstanding-request caps bound work; a cancelled request's late response is
//!   discarded (owner decision: the handler is not cancelled).

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use hpm_types::context::VerifiedHttpIdentity;
use rmcp::ServiceExt;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use super::json_error;
use super::limits::HttpLimits;
use crate::handler::McpHandler;

/// 6.2.0 `keepaliveIntervalMs` default.
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_millis(15_000);
const SESSION_REFUSED: &str = "Invalid or missing session ID";

/// FIFO admission with a bounded wait queue (6.2.0 `createSlotQueue`).
pub(crate) struct SlotQueue {
    slots: Arc<Semaphore>,
    waiting: AtomicUsize,
    max_waiting: usize,
}

pub(crate) enum Acquire {
    Granted(tokio::sync::OwnedSemaphorePermit),
    /// Queue full: the caller answers 429.
    Full,
    /// The queue was closed while waiting.
    Closed,
}

impl SlotQueue {
    pub(crate) fn new(max_active: usize, max_waiting: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(max_active)),
            waiting: AtomicUsize::new(0),
            max_waiting,
        }
    }

    pub(crate) async fn acquire(&self) -> Acquire {
        match self.slots.clone().try_acquire_owned() {
            Ok(p) => return Acquire::Granted(p),
            Err(tokio::sync::TryAcquireError::Closed) => return Acquire::Closed,
            Err(tokio::sync::TryAcquireError::NoPermits) => {}
        }
        if self
            .waiting
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                (w < self.max_waiting).then_some(w + 1)
            })
            .is_err()
        {
            return Acquire::Full;
        }
        let r = self.slots.clone().acquire_owned().await;
        self.waiting.fetch_sub(1, Ordering::AcqRel);
        r.map_or(Acquire::Closed, Acquire::Granted)
    }

    pub(crate) fn close(&self) {
        self.slots.close();
    }
}

fn is_json_rpc_id(id: &Value) -> bool {
    id.is_string() || id.as_f64().is_some_and(f64::is_finite)
}

/// `${typeof id}:${id}`.
fn request_id_key(id: &Value) -> String {
    match id {
        Value::String(s) => format!("string:{s}"),
        n => format!("number:{n}"),
    }
}

pub(crate) fn json_rpc_request_key(m: &Value) -> Option<String> {
    let o = m.as_object()?;
    let id = o.get("id")?;
    (o.get("method").is_some_and(Value::is_string) && is_json_rpc_id(id))
        .then(|| request_id_key(id))
}

pub(crate) fn json_rpc_response_key(m: &Value) -> Option<String> {
    let o = m.as_object()?;
    let id = o.get("id")?;
    (!o.contains_key("method")
        && is_json_rpc_id(id)
        && (o.contains_key("result") || o.contains_key("error")))
    .then(|| request_id_key(id))
}

pub(crate) fn cancelled_request_key(m: &Value) -> Option<String> {
    let o = m.as_object()?;
    if o.get("method").and_then(Value::as_str) != Some("notifications/cancelled")
        || o.contains_key("id")
    {
        return None;
    }
    let id = o.get("params")?.get("requestId")?;
    is_json_rpc_id(id).then(|| request_id_key(id))
}

struct Session {
    id: String,
    writer: tokio::sync::Mutex<WriteHalf<tokio::io::DuplexStream>>,
    posts: SlotQueue,
    outstanding: Mutex<HashSet<String>>,
    cancelled: Mutex<HashSet<String>>,
    initialized: AtomicBool,
    closed: CancellationToken,
    activity: Notify,
}

/// All legacy SSE sessions of one listener.
pub struct SseSessions {
    handler: McpHandler,
    limits: HttpLimits,
    keepalive: Duration,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    post_slots: SlotQueue,
    outstanding: AtomicUsize,
    shutting_down: AtomicBool,
}

impl SseSessions {
    pub fn new(handler: McpHandler, limits: HttpLimits, keepalive: Duration) -> Arc<Self> {
        Arc::new(Self {
            handler,
            post_slots: SlotQueue::new(limits.max_concurrent_posts, limits.max_active_sessions),
            limits,
            keepalive,
            sessions: Mutex::new(HashMap::new()),
            outstanding: AtomicUsize::new(0),
            shutting_down: AtomicBool::new(false),
        })
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().map_or(0, |s| s.len())
    }

    fn get(&self, id: &str) -> Option<Arc<Session>> {
        let s = self.sessions.lock().ok()?.get(id).cloned()?;
        (!s.closed.is_cancelled()).then_some(s)
    }

    /// Closes one session exactly once and releases its accounting.
    fn close(&self, session: &Session) {
        let removed = self
            .sessions
            .lock()
            .ok()
            .and_then(|mut s| s.remove(&session.id))
            .is_some();
        session.closed.cancel();
        session.posts.close();
        if removed {
            let n = session.outstanding.lock().map_or(0, |mut o| {
                let n = o.len();
                o.clear();
                n
            });
            self.outstanding.fetch_sub(n, Ordering::AcqRel);
            if let Ok(mut c) = session.cancelled.lock() {
                c.clear();
            }
        }
    }

    fn release_outstanding(&self, session: &Session, key: &str) {
        if session.outstanding.lock().is_ok_and(|mut o| o.remove(key)) {
            self.outstanding.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Stops admitting sessions and closes every live one (6.2.0 `closeAllSessions`).
    pub fn close_all(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.post_slots.close();
        let all: Vec<Arc<Session>> = self
            .sessions
            .lock()
            .map(|s| s.values().cloned().collect())
            .unwrap_or_default();
        for s in all {
            self.close(&s);
        }
    }

    /// `GET /mcp` with `Accept: text/event-stream`.
    pub fn open(self: &Arc<Self>, identity: VerifiedHttpIdentity) -> Response {
        if self.shutting_down.load(Ordering::Acquire) {
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down");
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (server_end, adapter_end) = tokio::io::duplex(1 << 16);
        let (adapter_r, adapter_w) = tokio::io::split(adapter_end);
        let session = Arc::new(Session {
            id: id.clone(),
            writer: tokio::sync::Mutex::new(adapter_w),
            posts: SlotQueue::new(1, self.limits.max_queued_posts_per_session),
            outstanding: Mutex::new(HashSet::new()),
            cancelled: Mutex::new(HashSet::new()),
            initialized: AtomicBool::new(false),
            closed: CancellationToken::new(),
            activity: Notify::new(),
        });
        {
            let Ok(mut map) = self.sessions.lock() else {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
            };
            let pending = map
                .values()
                .filter(|s| !s.initialized.load(Ordering::Acquire))
                .count();
            if pending >= self.limits.max_pending_sessions
                || map.len() >= self.limits.max_active_sessions
            {
                let mut r = json_error(StatusCode::TOO_MANY_REQUESTS, "Too many SSE sessions");
                r.headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
                return r;
            }
            map.insert(id.clone(), session.clone());
        }

        // Session lifecycle: setup (initialize) deadline, then idle and lifetime.
        let handler = self.handler.for_session(identity);
        let sessions = self.clone();
        let s = session.clone();
        tokio::spawn(async move {
            let limits = &sessions.limits;
            let lifetime = tokio::time::sleep(limits.session_max_lifetime);
            tokio::pin!(lifetime);
            let running = tokio::select! {
                r = handler.serve(server_end) => r.ok(),
                () = tokio::time::sleep(limits.session_setup_timeout) => None,
                () = s.closed.cancelled() => None,
            };
            if let Some(running) = running {
                s.initialized.store(true, Ordering::Release);
                loop {
                    tokio::select! {
                        () = s.activity.notified() => {}
                        () = tokio::time::sleep(limits.session_idle_timeout) => break,
                        () = &mut lifetime => break,
                        () = s.closed.cancelled() => break,
                    }
                }
                let _ = running.cancel().await;
            }
            sessions.close(&s);
        });

        // Server → client: one `message` event per JSON-RPC line.
        let (tx, rx) = mpsc::channel::<Event>(64);
        let endpoint = Event::default()
            .event("endpoint")
            .data(format!("/mcp?sessionId={id}"));
        let _ = tx.try_send(endpoint);
        let sessions = self.clone();
        let s = session.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(adapter_r).lines();
            loop {
                let line = tokio::select! {
                    l = lines.next_line() => l,
                    () = s.closed.cancelled() => break,
                };
                let Ok(Some(line)) = line else { break };
                s.activity.notify_one();
                if let Some(key) = serde_json::from_str::<Value>(&line)
                    .ok()
                    .as_ref()
                    .and_then(json_rpc_response_key)
                {
                    sessions.release_outstanding(&s, &key);
                    if s.cancelled.lock().is_ok_and(|mut c| c.remove(&key)) {
                        continue;
                    }
                }
                if tx
                    .send(Event::default().event("message").data(line))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            sessions.close(&s);
        });

        let stream = EventStream {
            rx,
            _guard: CloseOnDrop {
                sessions: self.clone(),
                session,
            },
        };
        let mut r = Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(self.keepalive).text("keepalive"))
            .into_response();
        let h = r.headers_mut();
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-transform"),
        );
        h.insert("x-accel-buffering", HeaderValue::from_static("no"));
        r
    }

    /// `POST /mcp?sessionId=<id>`. `body` is the raw request body.
    pub async fn post(&self, session_id: &str, headers: &HeaderMap, body: Bytes) -> Response {
        let Some(session) = self.get(session_id) else {
            // Recorded deviation (plan §5.3 rule 7): 404, 6.2.0 answered 400.
            return json_error(StatusCode::NOT_FOUND, SESSION_REFUSED);
        };
        let is_json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"));
        if !is_json {
            return json_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            );
        }
        let _turn = match session.posts.acquire().await {
            Acquire::Granted(p) => p,
            Acquire::Full => {
                return json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many queued requests for this session",
                );
            }
            Acquire::Closed => return json_error(StatusCode::NOT_FOUND, SESSION_REFUSED),
        };
        let _global = match self.post_slots.acquire().await {
            Acquire::Granted(p) => p,
            Acquire::Full => {
                return json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many concurrent requests",
                );
            }
            Acquire::Closed => return json_error(StatusCode::NOT_FOUND, SESSION_REFUSED),
        };
        if session.closed.is_cancelled() {
            return json_error(StatusCode::NOT_FOUND, SESSION_REFUSED);
        }
        session.activity.notify_one();
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            return json_error(StatusCode::BAD_REQUEST, "Invalid JSON body");
        };
        self.dispatch(&session, &message).await
    }

    async fn dispatch(&self, session: &Session, message: &Value) -> Response {
        if let Some(key) = cancelled_request_key(message)
            && session.outstanding.lock().is_ok_and(|o| o.contains(&key))
        {
            if let Ok(mut c) = session.cancelled.lock() {
                c.insert(key);
            }
            return (StatusCode::ACCEPTED, "Accepted").into_response();
        }
        let request_key = json_rpc_request_key(message);
        if let Some(key) = &request_key {
            let Ok(mut o) = session.outstanding.lock() else {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to process message",
                );
            };
            if o.contains(key) {
                return json_error(StatusCode::BAD_REQUEST, "Duplicate request id");
            }
            let global = self.outstanding.load(Ordering::Acquire);
            if o.len() >= self.limits.max_outstanding_requests_per_session
                || global >= self.limits.max_outstanding_requests
            {
                return json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many outstanding requests",
                );
            }
            o.insert(key.clone());
            self.outstanding.fetch_add(1, Ordering::AcqRel);
        }
        let mut line = message.to_string().into_bytes();
        line.push(b'\n');
        if session.writer.lock().await.write_all(&line).await.is_err() {
            if let Some(key) = &request_key {
                self.release_outstanding(session, key);
            }
            return json_error(StatusCode::NOT_FOUND, SESSION_REFUSED);
        }
        (StatusCode::ACCEPTED, "Accepted").into_response()
    }
}

/// Closes the session when the client drops the event stream.
struct CloseOnDrop {
    sessions: Arc<SseSessions>,
    session: Arc<Session>,
}

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.sessions.close(&self.session);
    }
}

struct EventStream {
    rx: mpsc::Receiver<Event>,
    _guard: CloseOnDrop,
}

impl futures_core::Stream for EventStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx).map(|e| e.map(Ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // http-server.test.js outstanding-id bookkeeping ('typeof:id' keys)
    #[test]
    fn json_rpc_keys() {
        assert_eq!(
            json_rpc_request_key(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})),
            Some("number:1".into())
        );
        assert_eq!(
            json_rpc_request_key(&json!({"id":"1","method":"x"})),
            Some("string:1".into())
        );
        assert_eq!(json_rpc_request_key(&json!({"id":null,"method":"x"})), None);
        assert_eq!(
            json_rpc_request_key(&json!({"method":"notifications/x"})),
            None
        );
        assert_eq!(json_rpc_request_key(&json!([{"id":1,"method":"x"}])), None);
        assert_eq!(
            json_rpc_response_key(&json!({"id":1,"result":{}})),
            Some("number:1".into())
        );
        assert_eq!(json_rpc_response_key(&json!({"id":1})), None);
        assert_eq!(
            json_rpc_response_key(&json!({"id":1,"method":"x","result":{}})),
            None
        );
        assert_eq!(
            cancelled_request_key(
                &json!({"method":"notifications/cancelled","params":{"requestId":"a"}})
            ),
            Some("string:a".into())
        );
        assert_eq!(
            cancelled_request_key(
                &json!({"id":2,"method":"notifications/cancelled","params":{"requestId":1}})
            ),
            None
        );
    }

    // createSlotQueue: one active, bounded waiters, close wakes waiters with false
    #[tokio::test]
    async fn slot_queue() {
        let q = Arc::new(SlotQueue::new(1, 1));
        let Acquire::Granted(first) = q.acquire().await else {
            panic!("first slot")
        };
        let q2 = q.clone();
        let waiter = tokio::spawn(async move { matches!(q2.acquire().await, Acquire::Granted(_)) });
        tokio::task::yield_now().await;
        while q.waiting.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        assert!(matches!(q.acquire().await, Acquire::Full));
        drop(first);
        assert!(waiter.await.expect("join"));
        let Acquire::Granted(_held) = q.acquire().await else {
            panic!("released slot")
        };
        let q3 = q.clone();
        let closed = tokio::spawn(async move { matches!(q3.acquire().await, Acquire::Closed) });
        while q.waiting.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        q.close();
        assert!(closed.await.expect("join"));
    }
}
