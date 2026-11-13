//! HTTP entry (port of `src/server.js`, the 6.2.0 `npm start` / Docker `CMD`):
//! configured only from the environment, serves Streamable HTTP and legacy SSE on
//! `/mcp` plus `/health` and `/instances`, every route behind the bearer guard.
//!
//! Like 6.2.0, progress goes to stdout and refusals to stderr; startup refusals
//! exit 1, a signal-driven shutdown exits 0.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hpm_core::config::{ConfigManager, Env, InstanceSummary};
use hpm_protocol::McpHandler;
use hpm_protocol::transport::limits::HttpSecurity;
use hpm_protocol::transport::{HttpApp, HttpConfig};
use hpm_types::context::{DeploymentMode, TrustedRoot};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;

use crate::{NAME, multi_thread_runtime, single_user_dispatcher, updates_hint};

const DEFAULT_PORT: u16 = 3000;
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_KEEPALIVE_MS: u64 = 15_000;
/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

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
    let code = runtime.block_on(run(&env, config, shutdown_signal()));
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

/// Resolves on the first SIGINT or SIGTERM, returning its name.
async fn wait_for_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                _ = term.recv() => "SIGTERM",
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

async fn shutdown_signal() {
    let signal = wait_for_signal().await;
    say(&format!("Received {signal}; closing MCP sessions"));
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
    let dispatcher = match single_user_dispatcher() {
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
    // `/instances` cannot answer 500 through `JsonFn`; a failed re-read is logged
    // and answered with the last list that loaded.
    let last_listed = std::sync::Mutex::new(match config.list_instances() {
        Ok(list) => summaries_json(&list),
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return 1;
        }
    });
    let listing = config.clone();
    let instances = Arc::new(move || {
        let mut last = last_listed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match listing.list_instances() {
            Ok(list) => *last = summaries_json(&list),
            Err(e) => eprintln!("HTTP request failed: {e}"),
        }
        last.clone()
    });
    let mut http = HttpConfig::new(security, health, instances);
    http.keepalive = keepalive;
    let app = HttpApp::new(
        McpHandler::new(
            TrustedRoot::streamable_http(DeploymentMode::SingleUser),
            dispatcher.clone(),
        ),
        McpHandler::new(
            TrustedRoot::legacy_sse(DeploymentMode::SingleUser),
            dispatcher,
        ),
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
    if env.get("DEBUG").as_deref() == Some("true") {
        say(&format!(
            "Active ServiceNow instance: {instance_name} - {instance_url}"
        ));
    }

    match app.serve(listener, shutdown).await {
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
    fn iso_timestamp() {
        assert_eq!(iso8601_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso8601_millis(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00.000Z"
        );
    }
}
