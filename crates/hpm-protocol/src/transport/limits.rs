//! Host/Origin checks, body and session limits (6.2.0 `src/http-server.js`
//! `DEFAULT_HTTP_LIMITS`, `parseAuthority`, `hostAllowed`, `originAllowed`).

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use axum::http::{HeaderMap, header};
use serde_json::Value;

use super::auth::ApiToken;

/// Startup configuration errors. Messages never echo the token.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// Resource bounds for the single-principal HTTP transports, enforced in-process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpLimits {
    pub max_pending_sessions: usize,
    pub max_active_sessions: usize,
    pub session_setup_timeout: Duration,
    pub session_idle_timeout: Duration,
    pub session_max_lifetime: Duration,
    pub max_json_bytes: usize,
    pub max_queued_posts_per_session: usize,
    pub max_concurrent_posts: usize,
    pub max_outstanding_requests_per_session: usize,
    pub max_outstanding_requests: usize,
    pub auth_failure_limit: u32,
    pub auth_failure_window: Duration,
    pub max_tracked_auth_peers: usize,
    /// Node `server.headersTimeout` (6.2.0 ran on the default): complete request
    /// headers must arrive within this, before any authentication.
    pub headers_timeout: Duration,
    /// Node `server.requestTimeout` default: the whole request body must arrive
    /// within this.
    pub request_timeout: Duration,
    /// Open TCP connections; further accepts are closed at once.
    pub max_connections: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_pending_sessions: 16,
            max_active_sessions: 32,
            session_setup_timeout: Duration::from_millis(30_000),
            session_idle_timeout: Duration::from_millis(30 * 60_000),
            session_max_lifetime: Duration::from_millis(12 * 60 * 60_000),
            max_json_bytes: 8 * 1024 * 1024,
            max_queued_posts_per_session: 8,
            max_concurrent_posts: 8,
            max_outstanding_requests_per_session: 16,
            max_outstanding_requests: 64,
            auth_failure_limit: 10,
            auth_failure_window: Duration::from_millis(60_000),
            max_tracked_auth_peers: 1024,
            headers_timeout: Duration::from_millis(60_000),
            request_timeout: Duration::from_millis(300_000),
            max_connections: 128,
        }
    }
}

impl HttpLimits {
    /// 6.2.0 `resolveLimits`: camelCase keys, positive safe integers only.
    pub fn with_overrides(overrides: &serde_json::Map<String, Value>) -> Result<Self, ConfigError> {
        const MAX_SAFE: u64 = (1 << 53) - 1;
        let mut l = Self::default();
        for (key, value) in overrides {
            let known = [
                "maxPendingSessions",
                "maxActiveSessions",
                "sessionSetupTimeoutMs",
                "sessionIdleTimeoutMs",
                "sessionMaxLifetimeMs",
                "maxJsonBytes",
                "maxQueuedPostsPerSession",
                "maxConcurrentPosts",
                "maxOutstandingRequestsPerSession",
                "maxOutstandingRequests",
                "authFailureLimit",
                "authFailureWindowMs",
                "maxTrackedAuthPeers",
                "headersTimeoutMs",
                "requestTimeoutMs",
                "maxConnections",
            ];
            if !known.contains(&key.as_str()) {
                return Err(ConfigError(format!("Unknown HTTP limit \"{key}\"")));
            }
            let n = value
                .as_u64()
                .filter(|n| *n > 0 && *n <= MAX_SAFE)
                .ok_or_else(|| {
                    ConfigError(format!("HTTP limit \"{key}\" must be a positive integer"))
                })?;
            let count = usize::try_from(n).unwrap_or(usize::MAX);
            let ms = Duration::from_millis(n);
            match key.as_str() {
                "maxPendingSessions" => l.max_pending_sessions = count,
                "maxActiveSessions" => l.max_active_sessions = count,
                "sessionSetupTimeoutMs" => l.session_setup_timeout = ms,
                "sessionIdleTimeoutMs" => l.session_idle_timeout = ms,
                "sessionMaxLifetimeMs" => l.session_max_lifetime = ms,
                "maxJsonBytes" => l.max_json_bytes = count,
                "maxQueuedPostsPerSession" => l.max_queued_posts_per_session = count,
                "maxConcurrentPosts" => l.max_concurrent_posts = count,
                "maxOutstandingRequestsPerSession" => {
                    l.max_outstanding_requests_per_session = count
                }
                "maxOutstandingRequests" => l.max_outstanding_requests = count,
                "authFailureLimit" => l.auth_failure_limit = u32::try_from(n).unwrap_or(u32::MAX),
                "authFailureWindowMs" => l.auth_failure_window = ms,
                "maxTrackedAuthPeers" => l.max_tracked_auth_peers = count,
                "headersTimeoutMs" => l.headers_timeout = ms,
                "requestTimeoutMs" => l.request_timeout = ms,
                _ => l.max_connections = count,
            }
        }
        Ok(l)
    }
}

const MAX_AUTHORITY_LENGTH: usize = 262;
const LOOPBACK_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authority {
    pub hostname: String,
    pub port: Option<u16>,
}

impl Authority {
    pub fn format(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.hostname),
            None => self.hostname.clone(),
        }
    }
}

fn format_ipv6(ip: &Ipv6Addr) -> String {
    format!("[{ip}]")
}

fn dns_label_ok(label: &str) -> bool {
    let b = label.as_bytes();
    let edge = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_';
    !b.is_empty()
        && b.len() <= 63
        && edge(b[0])
        && edge(b[b.len() - 1])
        && b.iter().all(|&c| edge(c) || c == b'-')
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`; lowercased; `None` when malformed.
pub fn parse_authority(value: &str) -> Option<Authority> {
    if value.is_empty() || value.len() > MAX_AUTHORITY_LENGTH {
        return None;
    }
    let (host_part, port_part, v6) = if let Some(rest) = value.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        if inner.is_empty()
            || !inner
                .bytes()
                .all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'.')
        {
            return None;
        }
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':')?),
        };
        (inner, port, true)
    } else {
        let (h, p) = match value.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (value, None),
        };
        if h.is_empty()
            || !h
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        {
            return None;
        }
        (h, p, false)
    };
    let port = match port_part {
        None => None,
        Some(p) => {
            let b = p.as_bytes();
            if b.is_empty() || b.len() > 5 || b[0] == b'0' || !b.iter().all(u8::is_ascii_digit) {
                return None;
            }
            Some(u16::try_from(p.parse::<u32>().ok()?).ok()?)
        }
    };
    if v6 {
        let ip: Ipv6Addr = host_part.parse().ok()?;
        return Some(Authority {
            hostname: format_ipv6(&ip),
            port,
        });
    }
    let hostname = host_part.to_ascii_lowercase();
    if hostname.bytes().all(|c| c.is_ascii_digit() || c == b'.') {
        hostname.parse::<Ipv4Addr>().ok()?;
        return Some(Authority { hostname, port });
    }
    if hostname.len() > 253 || !hostname.split('.').all(dns_label_ok) {
        return None;
    }
    Some(Authority { hostname, port })
}

pub fn parse_allowed_hosts(entries: &[String]) -> Result<Vec<String>, ConfigError> {
    let mut out: Vec<String> = Vec::new();
    for entry in entries {
        let a = parse_authority(entry).ok_or_else(|| {
            ConfigError(format!(
                "Invalid allowed host entry {}: expected host or host:port without scheme, path or wildcard",
                Value::String(entry.clone())
            ))
        })?;
        let f = a.format();
        if !out.contains(&f) {
            out.push(f);
        }
    }
    Ok(out)
}

/// Exact `scheme://host[:port]` origins: the entry must already be the WHATWG
/// serialized origin (lowercase, no default port, no path, no userinfo).
fn canonical_origin(entry: &str) -> bool {
    if entry.contains('*') {
        return false;
    }
    let Some((scheme, rest)) = entry.split_once("://") else {
        return false;
    };
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return false,
    };
    let Some(a) = parse_authority(rest) else {
        return false;
    };
    a.format() == rest && a.port != Some(default_port)
}

pub fn parse_allowed_origins(entries: &[String]) -> Result<Vec<String>, ConfigError> {
    let mut out: Vec<String> = Vec::new();
    for entry in entries {
        if !canonical_origin(entry) {
            return Err(ConfigError(format!(
                "Invalid allowed origin entry {}: expected an exact scheme://host[:port] origin without path or wildcard",
                Value::String(entry.clone())
            )));
        }
        if !out.contains(entry) {
            out.push(entry.clone());
        }
    }
    Ok(out)
}

fn split_list(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Validated HTTP security settings (6.2.0 `loadHttpSecurityConfig`).
#[derive(Clone, Debug)]
pub struct HttpSecurity {
    pub token: ApiToken,
    pub allowed_hosts: BTreeSet<String>,
    pub allowed_origins: BTreeSet<String>,
}

impl HttpSecurity {
    pub fn new(token: &str, hosts: &[String], origins: &[String]) -> Result<Self, ConfigError> {
        Ok(Self {
            token: ApiToken::parse(Some(token))?,
            allowed_hosts: parse_allowed_hosts(hosts)?.into_iter().collect(),
            allowed_origins: parse_allowed_origins(origins)?.into_iter().collect(),
        })
    }

    /// Reads `HAPPY_MCP_API_TOKEN`, `HAPPY_MCP_ALLOWED_HOSTS`, `HAPPY_MCP_ALLOWED_ORIGINS`.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let token = ApiToken::parse(get("HAPPY_MCP_API_TOKEN").as_deref())?;
        let named = |name: &str, r: Result<Vec<String>, ConfigError>| {
            r.map_err(|e| ConfigError(format!("{name}: {}", e.0)))
        };
        let hosts = named(
            "HAPPY_MCP_ALLOWED_HOSTS",
            parse_allowed_hosts(&split_list(get("HAPPY_MCP_ALLOWED_HOSTS"))),
        )?;
        let origins = named(
            "HAPPY_MCP_ALLOWED_ORIGINS",
            parse_allowed_origins(&split_list(get("HAPPY_MCP_ALLOWED_ORIGINS"))),
        )?;
        Ok(Self {
            token,
            allowed_hosts: hosts.into_iter().collect(),
            allowed_origins: origins.into_iter().collect(),
        })
    }
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn format_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format_ipv6(&v6),
    }
}

/// Exactly one Host naming an approved authority, or this listener's own address
/// and port (loopback names only on a loopback listener). Forwarded headers are
/// ignored.
pub fn host_allowed(headers: &HeaderMap, local: SocketAddr, approved: &BTreeSet<String>) -> bool {
    let mut values = headers.get_all(header::HOST).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    let Some(a) = value.to_str().ok().and_then(parse_authority) else {
        return false;
    };
    if approved.contains(&a.format()) {
        return true;
    }
    if a.port.unwrap_or(80) != local.port() {
        return false;
    }
    let ip = normalize_ip(local.ip());
    if a.hostname == format_ip(ip) {
        return true;
    }
    ip.is_loopback() && LOOPBACK_NAMES.contains(&a.hostname.as_str())
}

/// Absent Origin passes; otherwise exactly one approved origin.
pub fn origin_allowed(headers: &HeaderMap, approved: &BTreeSet<String>) -> bool {
    let mut values = headers.get_all(header::ORIGIN).iter();
    match (values.next(), values.next()) {
        (None, _) => true,
        (Some(v), None) => v.to_str().is_ok_and(|v| approved.contains(v)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    // http-server.test.js 'documents the enforced default limits'
    #[test]
    fn default_limits() {
        let l = HttpLimits::default();
        assert_eq!(l.max_pending_sessions, 16);
        assert_eq!(l.max_active_sessions, 32);
        assert_eq!(l.max_json_bytes, 8 * 1024 * 1024);
        assert_eq!(l.max_queued_posts_per_session, 8);
        assert_eq!(l.max_concurrent_posts, 8);
        assert_eq!(l.auth_failure_limit, 10);
        assert_eq!(l.auth_failure_window, Duration::from_millis(60_000));
    }

    // 'parses approved reverse-proxy authorities and browser origins'
    #[test]
    fn parses_hosts_and_origins() {
        let s = HttpSecurity::from_env(env(&[
            ("HAPPY_MCP_API_TOKEN", TOKEN),
            (
                "HAPPY_MCP_ALLOWED_HOSTS",
                " mcp.example.com, MCP.Example.com:8443,[::1]:3000,10.0.0.5:3000,mcp_upstream ",
            ),
            (
                "HAPPY_MCP_ALLOWED_ORIGINS",
                "https://mcp.example.com, http://localhost:5173",
            ),
        ]))
        .expect("valid");
        let hosts = parse_allowed_hosts(&split_list(Some(
            " mcp.example.com, MCP.Example.com:8443,[::1]:3000,10.0.0.5:3000,mcp_upstream ".into(),
        )))
        .expect("hosts");
        assert_eq!(
            hosts,
            [
                "mcp.example.com",
                "mcp.example.com:8443",
                "[::1]:3000",
                "10.0.0.5:3000",
                "mcp_upstream"
            ]
        );
        assert_eq!(s.allowed_hosts.len(), 5);
        assert!(s.allowed_origins.contains("https://mcp.example.com"));
        assert!(s.allowed_origins.contains("http://localhost:5173"));
        let empty = HttpSecurity::from_env(env(&[("HAPPY_MCP_API_TOKEN", TOKEN)])).expect("ok");
        assert!(empty.allowed_hosts.is_empty() && empty.allowed_origins.is_empty());
    }

    // 'rejects malformed or wildcard HAPPY_MCP_ALLOWED_HOSTS entry %p'
    #[test]
    fn rejects_bad_hosts() {
        for entry in [
            "*",
            "*.example.com",
            "http://mcp.example.com",
            "mcp.example.com/path",
            "user@mcp.example.com",
            "mcp.example.com:0",
            "mcp.example.com:65536",
            "mcp.example.com:03000",
            "[::1",
            "[not-ipv6]:3000",
            "bad host.example",
            "-bad.example",
            "mcp..example.com",
            "999.1.1.1",
        ] {
            let e = HttpSecurity::from_env(env(&[
                ("HAPPY_MCP_API_TOKEN", TOKEN),
                ("HAPPY_MCP_ALLOWED_HOSTS", entry),
            ]))
            .expect_err(entry);
            assert!(e.0.contains("HAPPY_MCP_ALLOWED_HOSTS"), "{entry}");
            let e = HttpSecurity::new(TOKEN, &[entry.into()], &[]).expect_err(entry);
            assert!(e.0.to_lowercase().contains("allowed host"), "{entry}");
        }
    }

    // 'rejects malformed or wildcard HAPPY_MCP_ALLOWED_ORIGINS entry %p'
    #[test]
    fn rejects_bad_origins() {
        for entry in [
            "*",
            "null",
            "https://*.example.com",
            "https://mcp.example.com/",
            "https://MCP.example.com",
            "mcp.example.com",
            "ftp://mcp.example.com",
            "https://user@mcp.example.com",
            "https://mcp.example.com/path",
        ] {
            let e = HttpSecurity::from_env(env(&[
                ("HAPPY_MCP_API_TOKEN", TOKEN),
                ("HAPPY_MCP_ALLOWED_ORIGINS", entry),
            ]))
            .expect_err(entry);
            assert!(e.0.contains("HAPPY_MCP_ALLOWED_ORIGINS"), "{entry}");
            let e = HttpSecurity::new(TOKEN, &[], &[entry.into()]).expect_err(entry);
            assert!(e.0.to_lowercase().contains("allowed origin"), "{entry}");
        }
    }

    // 'rejects %s limit overrides'
    #[test]
    fn rejects_bad_limits() {
        for o in [
            json!({"maxActiveSessions": 0}),
            json!({"maxJsonBytes": 1.5}),
            json!({"maxSessions": 1}),
        ] {
            let e = HttpLimits::with_overrides(o.as_object().expect("obj")).expect_err("bad");
            assert!(e.0.to_lowercase().contains("limit"));
        }
        let l =
            HttpLimits::with_overrides(json!({"authFailureWindowMs": 400}).as_object().expect("o"))
                .expect("ok");
        assert_eq!(l.auth_failure_window, Duration::from_millis(400));
    }

    fn hdr(name: header::HeaderName, values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(name.clone(), HeaderValue::from_str(v).expect("hv"));
        }
        h
    }

    // 'rejects %s in Host…', 'rejects duplicate Host headers…', 'accepts loopback names…'
    #[test]
    fn host_rules() {
        let local: SocketAddr = "127.0.0.1:4000".parse().expect("addr");
        let approved: BTreeSet<String> = ["mcp.example.com", "proxy.example.com:8443"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ok = |v: &[&str]| host_allowed(&hdr(header::HOST, v), local, &approved);
        for h in [
            "localhost:4000",
            "LOCALHOST:4000",
            "[::1]:4000",
            "127.0.0.1:4000",
            "mcp.example.com",
            "proxy.example.com:8443",
        ] {
            assert!(ok(&[h]), "{h}");
        }
        for h in [
            "evil.example",
            "evil.example:4000",
            "127.0.0.1:4001",
            "127.0.0.1",
            "127.0.0.1:4000/path",
            "*",
            "mcp.example.com:443",
            "proxy.example.com",
            "sub.mcp.example.com",
        ] {
            assert!(!ok(&[h]), "{h}");
        }
        assert!(!ok(&["127.0.0.1:4000", "127.0.0.1:4000"]));
        assert!(!ok(&[]));
        // A LAN listener accepts only its own address, never loopback names.
        let lan: SocketAddr = "10.1.2.3:4000".parse().expect("addr");
        let none = BTreeSet::new();
        assert!(host_allowed(
            &hdr(header::HOST, &["10.1.2.3:4000"]),
            lan,
            &none
        ));
        assert!(!host_allowed(
            &hdr(header::HOST, &["localhost:4000"]),
            lan,
            &none
        ));
        assert!(!host_allowed(&hdr(header::HOST, &["10.1.2.3"]), lan, &none));
    }

    // 'allows an absent Origin and exactly approved origins only'
    #[test]
    fn origin_rules() {
        let approved: BTreeSet<String> = ["https://console.example.com".to_string()].into();
        let ok = |v: &[&str]| origin_allowed(&hdr(header::ORIGIN, v), &approved);
        assert!(ok(&[]));
        assert!(ok(&["https://console.example.com"]));
        for o in [
            "null",
            "*",
            "https://evil.example",
            "https://console.example.com/",
            "https://console.example.com:443",
            "HTTPS://console.example.com",
            "not a url",
            "http://127.0.0.1:4000",
        ] {
            assert!(!ok(&[o]), "{o}");
        }
        assert!(!ok(&[
            "https://console.example.com",
            "https://console.example.com"
        ]));
    }
}
