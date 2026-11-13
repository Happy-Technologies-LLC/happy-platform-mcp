//! Opt-in, anonymous usage telemetry (6.2.0 `src/usage-telemetry.js`).
//!
//! Off unless `HAPPY_MCP_TELEMETRY` is `1`/`true`/`on`/`yes` (trimmed, any case). When
//! on, the tracker counts how many times each registered tool name is called and posts
//! those counts, with the package version, transport and OS platform, to
//! `HAPPY_MCP_TELEMETRY_URL` (default [`TELEMETRY_URL`]). Nothing about the ServiceNow
//! side is ever included. Sends are best effort: a 3 s timeout, every error swallowed,
//! never a retry, and [`UsageSink::record`] never blocks.
//!
//! The composition root builds one tracker from its [`Env`], gives it to
//! `Services::usage` and, on shutdown, awaits [`UsageTracker::flush`] (6.2.0 flushed on
//! `beforeExit`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hpm_core::config::{Env, PACKAGE_NAME, PACKAGE_VERSION};
use hpm_types::dispatch::UsageSink;
use serde_json::{Map, Value, json};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::logic::js::to_iso_string;

/// `src/project-links.js` `TELEMETRY_URL`.
pub const TELEMETRY_URL: &str = "https://build01-lab.tail0490c9.ts.net:10000/api/mcp/telemetry";

const FLUSH_INTERVAL: Duration = Duration::from_secs(10 * 60);
const FLUSH_THRESHOLD: u64 = 200;
const SEND_TIMEOUT: Duration = Duration::from_secs(3);

/// 6.2.0 `telemetryEnabled`.
pub fn telemetry_enabled(env: &Env) -> bool {
    let value = env.get("HAPPY_MCP_TELEMETRY").unwrap_or_default();
    matches!(
        value.trim().to_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// 6.2.0 `telemetryUrl`: the trimmed override, else the project default. Unlike 6.2.0,
/// an override must be `https`, or `http` to a loopback host (tests, local collectors):
/// anything else is an `Err` naming the problem, so counts never leave in plaintext.
pub fn telemetry_url(env: &Env) -> Result<String, String> {
    let value = env.get("HAPPY_MCP_TELEMETRY_URL").unwrap_or_default();
    let value = value.trim();
    if value.is_empty() {
        Ok(TELEMETRY_URL.to_owned())
    } else if url_allowed(value) {
        Ok(value.to_owned())
    } else {
        Err(format!(
            "HAPPY_MCP_TELEMETRY_URL must be an https URL (or http to a loopback host); got {}",
            redact_url(value)
        ))
    }
}

fn url_allowed(raw: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => url.host_str().is_some_and(|host| {
            let host = host.trim_matches(['[', ']']);
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }),
        _ => false,
    }
}

/// `raw` without userinfo, query or fragment, for messages and logs.
fn redact_url(raw: &str) -> String {
    match reqwest::Url::parse(raw) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => "(unparseable URL)".to_owned(),
    }
}

/// Node's `os.platform()` names.
fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// A random v4 session id (6.2.0 `randomUUID()`), from the OS random number generator.
fn session_id() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("OS random number generator unavailable");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[derive(Default)]
struct State {
    counts: BTreeMap<&'static str, u64>,
    transport: String,
    pending: u64,
    timer: Option<JoinHandle<()>>,
}

struct Inner {
    enabled: bool,
    url: String,
    session: String,
    state: Mutex<State>,
    /// One flush at a time; a later flush waits for the one in flight, then sends
    /// whatever was recorded since, so a shutdown flush never drops a batch.
    flush_lock: tokio::sync::Mutex<()>,
    /// Wakes the periodic flush so it exits without being aborted mid-send.
    stop: Arc<Notify>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 6.2.0 `createUsageTracker()`. Cheap to clone; clones share one counter set.
#[derive(Clone)]
pub struct UsageTracker {
    inner: Arc<Inner>,
}

impl UsageTracker {
    /// Reads the switch and the URL from `env`. A rejected URL override turns telemetry
    /// off (stderr note when it was asked for) rather than sending to the default.
    pub fn from_env(env: &Env) -> Self {
        let enabled = telemetry_enabled(env);
        match telemetry_url(env) {
            Ok(url) => Self::new(enabled, url),
            Err(note) => {
                if enabled {
                    eprintln!("Telemetry: off ({note})");
                }
                Self::new(false, TELEMETRY_URL)
            }
        }
    }

    pub fn new(enabled: bool, url: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                enabled,
                url: url.into(),
                session: session_id(),
                state: Mutex::new(State {
                    transport: "unknown".to_owned(),
                    ..State::default()
                }),
                flush_lock: tokio::sync::Mutex::new(()),
                stop: Arc::new(Notify::new()),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.enabled
    }

    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// Distinct tool names counted since the last flush.
    pub fn size(&self) -> usize {
        self.inner.state().counts.len()
    }

    /// 6.2.0 `setTransport`: an empty value is ignored.
    pub fn set_transport(&self, transport: &str) {
        if !transport.is_empty() {
            transport.clone_into(&mut self.inner.state().transport);
        }
    }

    /// The startup line (6.2.0 `describe()`), printed by both entry points when on: stderr
    /// for stdio, stdout for HTTP. The URL is shown without userinfo or query.
    pub fn describe(&self) -> String {
        if self.inner.enabled {
            format!(
                "Telemetry: on (anonymous tool-call counts to {}; set HAPPY_MCP_TELEMETRY=off to disable)",
                redact_url(&self.inner.url)
            )
        } else {
            "Telemetry: off".to_owned()
        }
    }

    /// 6.2.0 `buildPing()` payload, names sorted.
    pub fn build_ping(&self) -> Value {
        Self::ping(&self.inner, &self.inner.state())
    }

    fn ping(inner: &Inner, state: &State) -> Value {
        let tools: Map<String, Value> = state
            .counts
            .iter()
            .map(|(name, count)| ((*name).to_owned(), json!(count)))
            .collect();
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or_default();
        json!({
            "v": 1,
            "package": PACKAGE_NAME,
            "version": PACKAGE_VERSION,
            "session": inner.session,
            "transport": state.transport,
            // 6.2.0 reported the Node major; a Rust binary has none.
            "node": "unknown",
            "platform": platform(),
            "tools": tools,
            "sent_at": to_iso_string(millis).unwrap_or_default(),
        })
    }

    /// Posts and clears the counts. Best effort: returns whether a request completed;
    /// nothing is surfaced and nothing is retried. A flush already in flight is awaited
    /// first, and the whole call is bounded (two send timeouts plus a second).
    pub async fn flush(&self) -> bool {
        tokio::time::timeout(
            SEND_TIMEOUT * 2 + Duration::from_secs(1),
            Self::flush_inner(&self.inner),
        )
        .await
        .unwrap_or(false)
    }

    async fn flush_inner(inner: &Inner) -> bool {
        if !inner.enabled {
            return false;
        }
        let _flushing = inner.flush_lock.lock().await;
        let body = {
            let mut state = inner.state();
            if state.counts.is_empty() {
                return false;
            }
            let body = Self::ping(inner, &state).to_string();
            state.counts.clear();
            state.pending = 0;
            body
        };
        let Ok(client) = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
        else {
            return false;
        };
        client
            .post(&inner.url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .is_ok()
    }

    /// 6.2.0 `stop()`: ends the periodic flush. A send already running is not cut off;
    /// [`UsageTracker::flush`] waits for it.
    pub fn stop(&self) {
        if self.inner.state().timer.take().is_some() {
            self.inner.stop.notify_one();
        }
    }

    fn start_timer(&self, state: &mut State) {
        if state.timer.is_some() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // Weak: the timer must not keep an abandoned tracker alive.
        let inner = Arc::downgrade(&self.inner);
        let stop = Arc::clone(&self.inner.stop);
        state.timer = Some(runtime.spawn(async move {
            // A notification (stop) ends the loop; a timeout is the next interval.
            while tokio::time::timeout(FLUSH_INTERVAL, stop.notified())
                .await
                .is_err()
            {
                let Some(inner) = inner.upgrade() else { break };
                Self::flush_inner(&inner).await;
            }
        }));
    }
}

impl UsageSink for UsageTracker {
    fn record(&self, tool: &'static str) {
        if !self.inner.enabled {
            return;
        }
        let name = if tool.is_empty() { "unknown" } else { tool };
        let flush_now = {
            let mut state = self.inner.state();
            *state.counts.entry(name).or_insert(0) += 1;
            state.pending += 1;
            self.start_timer(&mut state);
            state.pending >= FLUSH_THRESHOLD
        };
        if flush_now && let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let inner = Arc::clone(&self.inner);
            drop(runtime.spawn(async move { Self::flush_inner(&inner).await }));
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env::from_pairs(pairs)
    }

    #[test]
    fn switch_accepts_only_the_on_values_trimmed_any_case() {
        for on in ["1", "true", "ON", " Yes ", "TRUE"] {
            assert!(
                telemetry_enabled(&env(&[("HAPPY_MCP_TELEMETRY", on)])),
                "{on}"
            );
        }
        for off in ["", "0", "off", "false", "enabled"] {
            assert!(
                !telemetry_enabled(&env(&[("HAPPY_MCP_TELEMETRY", off)])),
                "{off}"
            );
        }
        assert!(!telemetry_enabled(&env(&[])));
    }

    #[test]
    fn url_is_the_trimmed_override_or_the_project_default() {
        assert_eq!(telemetry_url(&env(&[])).unwrap(), TELEMETRY_URL);
        assert_eq!(
            telemetry_url(&env(&[("HAPPY_MCP_TELEMETRY_URL", "  ")])).unwrap(),
            TELEMETRY_URL
        );
        assert_eq!(
            telemetry_url(&env(&[("HAPPY_MCP_TELEMETRY_URL", " https://x.test/p ")])).unwrap(),
            "https://x.test/p"
        );
    }

    #[test]
    fn override_must_be_https_or_loopback_http() {
        for ok in [
            "https://collector.example/t",
            "http://127.0.0.1:9/t",
            "http://localhost:9/t",
            "http://[::1]:9/t",
        ] {
            assert!(
                telemetry_url(&env(&[("HAPPY_MCP_TELEMETRY_URL", ok)])).is_ok(),
                "{ok}"
            );
        }
        for bad in [
            "http://collector.example/t",
            "ftp://127.0.0.1/t",
            "file:///tmp/x",
            "collector.example/t",
            "https://",
        ] {
            assert!(
                telemetry_url(&env(&[("HAPPY_MCP_TELEMETRY_URL", bad)])).is_err(),
                "{bad}"
            );
        }
        // A rejected override disables telemetry; it never falls back to the default.
        let tracker = UsageTracker::from_env(&env(&[
            ("HAPPY_MCP_TELEMETRY", "1"),
            ("HAPPY_MCP_TELEMETRY_URL", "http://collector.example/t"),
        ]));
        assert!(!tracker.enabled());
    }

    #[test]
    fn rejection_note_does_not_echo_credentials_or_query() {
        let note = telemetry_url(&env(&[(
            "HAPPY_MCP_TELEMETRY_URL",
            "http://user:secret@collector.example/t?token=abc",
        )]))
        .unwrap_err();
        assert!(!note.contains("secret") && !note.contains("abc"), "{note}");
    }

    #[test]
    fn describe_redacts_userinfo_and_query() {
        let tracker = UsageTracker::new(true, "https://user:secret@x.test/p?token=abc#frag");
        let line = tracker.describe();
        assert!(line.contains("https://x.test/p;"), "{line}");
        assert!(
            !line.contains("secret") && !line.contains("token"),
            "{line}"
        );
    }

    #[test]
    fn describe_names_the_url_when_on() {
        let on = UsageTracker::new(true, "http://x.test/p");
        assert_eq!(
            on.describe(),
            "Telemetry: on (anonymous tool-call counts to http://x.test/p; set HAPPY_MCP_TELEMETRY=off to disable)"
        );
        assert_eq!(UsageTracker::new(false, "u").describe(), "Telemetry: off");
    }

    #[test]
    fn disabled_tracker_counts_nothing() {
        let tracker = UsageTracker::new(false, "u");
        tracker.record("SN-Query-Table");
        assert_eq!(tracker.size(), 0);
    }

    #[test]
    fn ping_has_the_documented_shape_with_sorted_tool_counts() {
        let tracker = UsageTracker::new(true, "u");
        tracker.set_transport("stdio");
        tracker.set_transport("");
        for name in ["SN-Query-Table", "SN-Create-Record", "SN-Query-Table"] {
            tracker.record(name);
        }
        let ping = tracker.build_ping();
        let mut keys: Vec<_> = ping.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "node",
                "package",
                "platform",
                "sent_at",
                "session",
                "tools",
                "transport",
                "v",
                "version"
            ]
        );
        assert_eq!(ping["v"], 1);
        assert_eq!(ping["transport"], "stdio");
        assert_eq!(ping["package"], PACKAGE_NAME);
        assert_eq!(
            ping["tools"].to_string(),
            r#"{"SN-Create-Record":1,"SN-Query-Table":2}"#
        );
        let session = ping["session"].as_str().unwrap();
        assert_eq!(session.len(), 36);
        assert_eq!(&session[14..15], "4");
    }

    #[tokio::test]
    async fn flush_posts_the_ping_once_and_clears_the_counts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/telemetry", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buf = [0_u8; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                received.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&received).into_owned();
                if let Some(at) = text.find("\r\n\r\n") {
                    let length = text[..at]
                        .to_ascii_lowercase()
                        .split("content-length: ")
                        .nth(1)
                        .and_then(|rest| rest.split("\r\n").next()?.parse::<usize>().ok())
                        .unwrap_or(0);
                    if received.len() >= at + 4 + length {
                        break;
                    }
                }
                assert!(n > 0, "connection closed early");
            }
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(received).unwrap()
        });
        let tracker = UsageTracker::new(true, url);
        tracker.record("SN-Docs-Get");
        assert!(tracker.flush().await);
        assert_eq!(tracker.size(), 0);
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /telemetry HTTP/1.1"), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert!(
            request.contains(r#""tools":{"SN-Docs-Get":1}"#),
            "{request}"
        );
        // Nothing left to send.
        assert!(!tracker.flush().await);
    }

    #[tokio::test]
    async fn off_by_default_sends_nothing() {
        // No `HAPPY_MCP_TELEMETRY` in the environment, a live listener as the URL.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let env = env(&[("HAPPY_MCP_TELEMETRY_URL", url.as_str())]);
        let tracker = UsageTracker::from_env(&env);
        assert!(!tracker.enabled());
        assert_eq!(tracker.describe(), "Telemetry: off");
        for _ in 0..FLUSH_THRESHOLD + 1 {
            tracker.record("SN-Docs-Get");
        }
        assert_eq!(tracker.size(), 0);
        assert!(!tracker.flush().await);
        let accepted = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
        assert!(accepted.is_err(), "a disabled tracker must not connect");
    }

    #[tokio::test]
    async fn the_flush_threshold_posts_without_waiting_for_the_timer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let tracker = UsageTracker::new(true, url);
        for _ in 0..FLUSH_THRESHOLD {
            tracker.record("SN-Docs-Get");
        }
        let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
        assert!(accepted.is_ok(), "the 200th call flushes");
        tracker.stop();
    }

    /// Reads one HTTP request (headers plus `content-length` body) from `socket`.
    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut received = Vec::new();
        let mut buf = [0_u8; 4096];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            received.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&received).into_owned();
            if let Some(at) = text.find("\r\n\r\n") {
                let length = text[..at]
                    .to_ascii_lowercase()
                    .split("content-length: ")
                    .nth(1)
                    .and_then(|rest| rest.split("\r\n").next()?.parse::<usize>().ok())
                    .unwrap_or(0);
                if received.len() >= at + 4 + length {
                    return text;
                }
            }
            assert!(n > 0, "connection closed early");
        }
    }

    #[tokio::test]
    async fn a_stalled_endpoint_bounds_the_flush() {
        // Accepts and reads, never answers.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let tracker = UsageTracker::new(true, url);
        tracker.record("SN-Docs-Get");
        let started = std::time::Instant::now();
        assert!(!tracker.flush().await);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        tracker.stop();
        server.abort();
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_url = format!("http://{}/elsewhere", target.local_addr().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            let reply = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: {target_url}\r\ncontent-length: 0\r\n\r\n"
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        let tracker = UsageTracker::new(true, url);
        tracker.record("SN-Docs-Get");
        assert!(
            tracker.flush().await,
            "the 307 itself is a completed request"
        );
        server.await.unwrap();
        let followed = tokio::time::timeout(Duration::from_millis(300), target.accept()).await;
        assert!(
            followed.is_err(),
            "the redirect target must not be contacted"
        );
    }

    #[tokio::test]
    async fn shutdown_flush_waits_for_the_flush_in_flight() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for first in [true, false] {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut socket).await);
                if first {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                socket
                    .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                    .await
                    .unwrap();
            }
            requests
        });
        let tracker = UsageTracker::new(true, url);
        for _ in 0..FLUSH_THRESHOLD {
            tracker.record("SN-Docs-Get");
        }
        // The threshold flush is now sending its 200 calls (the server delays its reply).
        tokio::time::sleep(Duration::from_millis(100)).await;
        tracker.record("SN-Docs-Search");
        tracker.stop();
        assert!(tracker.flush().await, "the shutdown flush sends the rest");
        let requests = server.await.unwrap();
        assert!(
            requests[0].contains(r#""SN-Docs-Get":200"#),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].contains(r#""tools":{"SN-Docs-Search":1}"#),
            "{}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn send_errors_are_swallowed() {
        // A port nothing listens on.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let tracker = UsageTracker::new(true, url);
        tracker.record("SN-Docs-Get");
        assert!(!tracker.flush().await);
        assert_eq!(tracker.size(), 0);
        tracker.stop();
    }
}
