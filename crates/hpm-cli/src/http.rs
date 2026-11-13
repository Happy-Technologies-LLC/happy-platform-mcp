//! HTTP entry (port of `src/server.js`, the 6.2.0 `npm start` / Docker `CMD`):
//! configured only from the environment, serves Streamable HTTP and legacy SSE on
//! `/mcp` plus `/health` and `/instances`, every route behind the bearer guard.
//!
//! Like 6.2.0, progress goes to stdout and refusals to stderr; startup refusals
//! exit 1, a signal-driven shutdown exits 0 (a second signal during the
//! [`SHUTDOWN_GRACE`] window exits at once with 130 for SIGINT, 143 for SIGTERM).

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hpm_core::config::{ConfigManager, Env, InstanceSummary};
use hpm_protocol::McpHandler;
use hpm_protocol::transport::limits::HttpSecurity;
use hpm_protocol::transport::{HttpApp, HttpConfig, InstancesFn};
use hpm_tools::families::docs::telemetry::UsageTracker;
use hpm_types::context::{DeploymentMode, TrustedRoot};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;

use crate::{NAME, multi_thread_runtime, single_user_dispatcher, updates_hint};

const DEFAULT_PORT: u16 = 3000;
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_KEEPALIVE_MS: u64 = 15_000;
/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
/// How long open connections may finish after the first SIGINT/SIGTERM, and the
/// bound on runtime teardown (blocking tasks) after the serve loop ends. Kept
/// well under Docker's 10 s stop timeout.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Process start of every binary: loads the restricted `.env` of the working
/// directory (P4-3; 6.2.0 `dotenv.config()` in `server.js` and `stdio-server.js`)
/// and prints its warnings to stderr, never stdout (stdio carries the protocol).
pub fn load_dotenv() {
    let Ok(dir) = std::env::current_dir() else {
        return;
    };
    for warning in hpm_core::config::install_dotenv(&dir) {
        eprintln!("{NAME}: {warning}");
    }
}

/// Process entry of `happy-platform-mcp-http`. Arguments are ignored, as `node
/// src/server.js` ignores them.
pub fn main() -> ExitCode {
    let runtime = match multi_thread_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let env = Env::process();
    let config = Arc::new(ConfigManager::new(
        Arc::new(hpm_core::registry::InstanceRegistry::new(env.clone())),
        env.clone(),
    ));
    let (forced_tx, forced_rx) = tokio::sync::oneshot::channel();
    let (code, forced) = runtime.block_on(async {
        // Registered before serving, so no early signal is lost.
        let shutdown = shutdown_signal(Signals::new(), forced_tx);
        tokio::select! {
            code = run(&env, config, shutdown) => (code, false),
            Ok(signal) = forced_rx => {
                eprintln!("Received {signal} again; exiting immediately");
                (forced_exit_code(signal), true)
            }
        }
    });
    // Never wait unboundedly for `spawn_blocking` work (e.g. a hung keychain call);
    // a forced exit does not wait at all.
    runtime.shutdown_timeout(if forced {
        Duration::ZERO
    } else {
        SHUTDOWN_GRACE
    });
    ExitCode::from(code)
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn refuse(message: &str) -> u8 {
    eprintln!("Happy MCP Server HTTP startup refused: {message}");
    1
}

/// `Number(value || fallback)` restricted to what 6.2.0 then accepts.
fn js_number(value: Option<String>) -> Option<f64> {
    match value.as_deref().map(str::trim) {
        None => None,
        Some("") => Some(0.0),
        Some(v) => Some(v.parse::<f64>().ok().filter(|n| n.is_finite())?),
    }
}

/// `PORT` (default 3000): an integer 0–65535; 0 picks an ephemeral port.
pub fn parse_port(value: Option<String>) -> Result<u16, String> {
    if value.as_deref().is_none_or(str::is_empty) {
        return Ok(DEFAULT_PORT);
    }
    js_number(value)
        .filter(|n| n.fract() == 0.0 && (0.0..=65535.0).contains(n))
        .map(|n| n as u16)
        .ok_or_else(|| "PORT must be an integer between 0 and 65535".to_string())
}

/// `SSE_KEEPALIVE_INTERVAL` in ms (default 15000): a positive safe integer.
pub fn parse_keepalive(value: Option<String>) -> Result<Duration, String> {
    if value.as_deref().is_none_or(str::is_empty) {
        return Ok(Duration::from_millis(DEFAULT_KEEPALIVE_MS));
    }
    js_number(value)
        .filter(|n| n.fract() == 0.0 && *n > 0.0 && *n <= MAX_SAFE_INTEGER)
        .map(|n| Duration::from_millis(n as u64))
        .ok_or_else(|| "keepaliveIntervalMs must be a positive integer".to_string())
}

/// `HAPPY_MCP_BIND_HOST` (default 127.0.0.1).
pub fn bind_host(value: Option<String>) -> String {
    value
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_HOST.to_string())
}

/// `new Date().toISOString()`.
fn iso8601_millis(time: SystemTime) -> String {
    let millis_total = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let millis = u32::try_from(millis_total % 1000).unwrap_or(0);
    let seconds = i64::try_from(millis_total / 1000).unwrap_or(i64::MAX / 2);
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// `${value}` of a JSON field as `console.log` would print it.
fn display(o: &Map<String, Value>, key: &str) -> String {
    match o.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "undefined".to_string(),
    }
}

fn summaries_json(list: &[InstanceSummary]) -> Value {
    Value::Array(
        list.iter()
            .map(|i| {
                json!({
                    "name": i.name,
                    "url": i.url,
                    "default": i.default,
                    "description": i.description,
                })
            })
            .collect(),
    )
}

/// The two HTTP trusted roots, in `HttpApp::new` order: (Streamable HTTP, legacy SSE).
pub fn http_roots() -> (TrustedRoot, TrustedRoot) {
    (
        TrustedRoot::streamable_http(DeploymentMode::SingleUser),
        TrustedRoot::legacy_sse(DeploymentMode::SingleUser),
    )
}

/// Process signal stream: SIGINT and SIGTERM (Ctrl-C only off unix).
struct Signals {
    #[cfg(unix)]
    int: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                int: signal(SignalKind::interrupt()).ok(),
                term: signal(SignalKind::terminate()).ok(),
            }
        }
        #[cfg(not(unix))]
        Self {}
    }

    /// Resolves on the next SIGINT or SIGTERM, returning its name.
    async fn next(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            async fn recv(s: &mut Option<tokio::signal::unix::Signal>) {
                match s {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending().await,
                }
            }
            tokio::select! {
                () = recv(&mut self.int) => "SIGINT",
                () = recv(&mut self.term) => "SIGTERM",
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            "SIGINT"
        }
    }
}

/// Exit code of a forced exit: 128 + signal number, as a shell reports it.
fn forced_exit_code(signal: &str) -> u8 {
    if signal == "SIGTERM" { 143 } else { 130 }
}

/// The first SIGINT/SIGTERM starts a graceful shutdown (exit 0 once sessions close,
/// at most [`SHUTDOWN_GRACE`] later). A second signal during that window is sent
/// on `forced`, and `main` exits immediately with 130 (SIGINT) or 143 (SIGTERM).
async fn shutdown_signal(mut signals: Signals, forced: tokio::sync::oneshot::Sender<&'static str>) {
    let signal = signals.next().await;
    say(&format!("Received {signal}; closing MCP sessions"));
    tokio::spawn(async move {
        let again = signals.next().await;
        let _ = forced.send(again);
    });
}

/// `server.js` start-up and serve loop; returns the exit code. `shutdown` ends the
/// serve loop (signals in production).
pub async fn run(
    env: &Env,
    config: Arc<ConfigManager>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> u8 {
    let port = parse_port(env.get("PORT"));
    let host = bind_host(env.get("HAPPY_MCP_BIND_HOST"));
    let keepalive = parse_keepalive(env.get("SSE_KEEPALIVE_INTERVAL"));

    let security = match HttpSecurity::from_env(|k| env.get(k)) {
        Ok(s) => s,
        Err(e) => return refuse(&e.0),
    };

    let default_instance = match config.get_default_instance() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    };
    let instance_name = display(&default_instance, "name");
    let instance_url = display(&default_instance, "url");
    say(&format!(
        "Default ServiceNow instance: {instance_name} ({instance_url})"
    ));

    let keepalive = match keepalive {
        Ok(k) => k,
        Err(e) => return refuse(&e),
    };
    let usage = UsageTracker::from_env(env);
    usage.set_transport("http");
    let dispatcher = match single_user_dispatcher(config.clone(), &usage) {
        Ok(d) => d,
        Err(e) => return refuse(&e),
    };
    let health_name = default_instance.get("name").cloned();
    let health_url = default_instance.get("url").cloned();
    let health = Arc::new(move || {
        let mut body = Map::new();
        body.insert("status".into(), json!("healthy"));
        if let Some(url) = &health_url {
            body.insert("servicenow_instance".into(), url.clone());
        }
        if let Some(name) = &health_name {
            body.insert("instance_name".into(), name.clone());
        }
        body.insert("timestamp".into(), json!(iso8601_millis(SystemTime::now())));
        Value::Object(body)
    });
    // Startup fails if the registry cannot be listed; afterwards each request
    // re-reads it off the runtime, and a failure answers 500 like 6.2.0's error
    // boundary.
    if let Err(e) = config.list_instances() {
        eprintln!("{NAME}: {e}");
        return 1;
    }
    let listing = config.clone();
    let instances: InstancesFn = Arc::new(move || {
        let listing = listing.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || listing.list_instances()).await {
                Ok(Ok(list)) => Ok(summaries_json(&list)),
                Ok(Err(e)) => {
                    eprintln!("HTTP request failed: {e}");
                    Err(())
                }
                Err(e) => {
                    eprintln!("HTTP request failed: {e}");
                    Err(())
                }
            }
        })
    });
    let mut http = HttpConfig::new(security, health, instances);
    http.keepalive = keepalive;
    http.shutdown_grace = SHUTDOWN_GRACE;
    let (streamable_root, sse_root) = http_roots();
    let app = HttpApp::new(
        McpHandler::builder(streamable_root, dispatcher.clone())
            .registry(config.clone())
            .build(),
        McpHandler::builder(sse_root, dispatcher)
            .registry(config.clone())
            .build(),
        http,
    );

    let port = match port {
        Ok(p) => p,
        Err(e) => return refuse(&e),
    };
    let listener = match TcpListener::bind((host.as_str(), port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{NAME}: listen on {host}:{port} failed: {e}");
            return 1;
        }
    };
    let port = listener.local_addr().map_or(port, |a| a.port());
    say(&format!(
        "Happy MCP Server listening on http://{host}:{port} (bearer authentication required)"
    ));
    say(&format!("Health check: http://{host}:{port}/health"));
    say(&format!("MCP SSE endpoint: http://{host}:{port}/mcp"));
    say(&format!(
        "Available instances: http://{host}:{port}/instances"
    ));
    say(&updates_hint());
    if usage.enabled() {
        say(&usage.describe());
    }
    if env.get("DEBUG").as_deref() == Some("true") {
        say(&format!(
            "Active ServiceNow instance: {instance_name} - {instance_url}"
        ));
    }

    let served = app.serve(listener, shutdown).await;
    usage.stop();
    usage.flush().await;
    match served {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn port_defaults_and_validation() {
        assert_eq!(parse_port(None), Ok(3000));
        assert_eq!(parse_port(s("")), Ok(3000));
        assert_eq!(parse_port(s("0")), Ok(0));
        assert_eq!(parse_port(s(" 8080 ")), Ok(8080));
        assert_eq!(parse_port(s("65535")), Ok(65535));
        for bad in ["65536", "-1", "80.5", "abc", "Infinity"] {
            assert_eq!(
                parse_port(s(bad)),
                Err("PORT must be an integer between 0 and 65535".to_string()),
                "{bad}"
            );
        }
    }

    #[test]
    fn keepalive_defaults_and_validation() {
        assert_eq!(parse_keepalive(None), Ok(Duration::from_millis(15_000)));
        assert_eq!(parse_keepalive(s("250")), Ok(Duration::from_millis(250)));
        for bad in ["0", "-5", "1.5", "x", " "] {
            assert_eq!(
                parse_keepalive(s(bad)),
                Err("keepaliveIntervalMs must be a positive integer".to_string()),
                "{bad}"
            );
        }
    }

    #[test]
    fn bind_host_default() {
        assert_eq!(bind_host(None), "127.0.0.1");
        assert_eq!(bind_host(s("")), "127.0.0.1");
        assert_eq!(bind_host(s("0.0.0.0")), "0.0.0.0");
    }

    #[test]
    fn each_transport_gets_its_own_root() {
        use hpm_types::context::TransportKind;
        let (streamable, sse) = http_roots();
        assert_eq!(streamable.transport(), TransportKind::StreamableHttp);
        assert_eq!(sse.transport(), TransportKind::LegacySse);
        assert_eq!(streamable.mode(), DeploymentMode::SingleUser);
        assert_eq!(sse.mode(), DeploymentMode::SingleUser);
        assert_eq!(forced_exit_code("SIGINT"), 130);
        assert_eq!(forced_exit_code("SIGTERM"), 143);
    }

    #[test]
    fn iso_timestamp() {
        assert_eq!(iso8601_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso8601_millis(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00.000Z"
        );
    }
}
