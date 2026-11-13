//! Single-user bearer token and the per-peer authentication-failure limiter
//! (6.2.0 `validateApiToken`, `bearerMatches`, `createAuthFailureLimiter`).

use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header};
use hpm_types::context::{ClientIdentity, Principal, VerifiedHttpIdentity};

use sha2::{Digest, Sha256};

use super::limits::ConfigError;

const TOKEN_GUIDANCE: &str = "generate one with `openssl rand -hex 32`";

pub const API_TOKEN_CLIENT_ID: &str = "happy-mcp-api-token";

/// 32 random bytes as 64 hex or 43 canonical base64url characters.
fn well_formed(token: &str) -> bool {
    let b = token.as_bytes();
    if b.len() == 64 && b.iter().all(u8::is_ascii_hexdigit) {
        return true;
    }
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    if b.len() != 43 || !b.iter().all(|c| ALPHABET.contains(c)) {
        return false;
    }
    // 43 chars carry 258 bits; canonical encoding leaves the last 2 bits zero.
    let last = ALPHABET.iter().position(|c| *c == b[42]).unwrap_or(0);
    last & 0b11 == 0
}

/// The configured secret. `Debug` never prints it.
#[derive(Clone)]
pub struct ApiToken(Vec<u8>);

impl fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiToken(<redacted>)")
    }
}

impl ApiToken {
    pub fn parse(token: Option<&str>) -> Result<Self, ConfigError> {
        let token = token.filter(|t| !t.is_empty()).ok_or_else(|| {
            ConfigError(format!(
                "HAPPY_MCP_API_TOKEN is required for the HTTP transport; {TOKEN_GUIDANCE}"
            ))
        })?;
        if !well_formed(token) {
            return Err(ConfigError(format!(
                "HAPPY_MCP_API_TOKEN must be 32 random bytes encoded as 64 hex or 43 base64url characters; {TOKEN_GUIDANCE}"
            )));
        }
        Ok(Self(token.as_bytes().to_vec()))
    }

    /// Exactly one `Authorization: Bearer <token>`, compared in constant time.
    pub fn matches(&self, headers: &HeaderMap) -> bool {
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let (Some(value), None) = (values.next(), values.next()) else {
            return false;
        };
        let Ok(value) = value.to_str() else {
            return false;
        };
        let Some((scheme, presented)) = value.split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("bearer") || !well_formed(presented) {
            return false;
        }
        let (a, b) = (presented.as_bytes(), self.0.as_slice());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

/// The identity every authenticated single-user HTTP request carries. The only
/// place the single-user bearer principal is built; sessions bind to the SHA-256
/// fingerprint of the token, never the token.
pub fn operator_identity(token: &ApiToken) -> VerifiedHttpIdentity {
    VerifiedHttpIdentity::new(
        Principal::LocalBearer {
            token_fingerprint: Sha256::digest(&token.0).into(),
        },
        ClientIdentity {
            client_id: API_TOKEN_CLIENT_ID.into(),
            verified: true,
        },
        Vec::<String>::new().into(),
    )
}

struct Entry {
    peer: String,
    window_start: Instant,
    failures: u32,
}

/// Fixed-window failure counter per remote peer, bounded in tracked peers.
pub struct AuthFailureLimiter {
    limit: u32,
    window: Duration,
    max_peers: usize,
    /// Window-start order, like the JS `Map` insertion order.
    peers: Mutex<VecDeque<Entry>>,
}

impl AuthFailureLimiter {
    pub fn new(limit: u32, window: Duration, max_peers: usize) -> Self {
        Self {
            limit,
            window,
            max_peers,
            peers: Mutex::new(VecDeque::new()),
        }
    }

    fn expired(&self, e: &Entry, now: Instant) -> bool {
        now.duration_since(e.window_start) >= self.window
    }

    pub fn size(&self) -> usize {
        self.peers.lock().map_or(0, |p| p.len())
    }

    pub fn retry_after_seconds(&self, peer: &str, now: Instant) -> Option<u64> {
        let mut peers = self.peers.lock().ok()?;
        let idx = peers.iter().position(|e| e.peer == peer)?;
        if self.expired(&peers[idx], now) {
            peers.remove(idx);
            return None;
        }
        let e = &peers[idx];
        if e.failures < self.limit {
            return None;
        }
        let left = (e.window_start + self.window).saturating_duration_since(now);
        Some(left.as_millis().div_ceil(1000).max(1) as u64)
    }

    /// `false` when the peer table is full of live windows (caller answers 429).
    pub fn record_failure(&self, peer: &str, now: Instant) -> bool {
        let Ok(mut peers) = self.peers.lock() else {
            return false;
        };
        if let Some(idx) = peers.iter().position(|e| e.peer == peer) {
            if self.expired(&peers[idx], now) {
                peers.remove(idx);
            } else {
                peers[idx].failures += 1;
                return true;
            }
        }
        if peers.len() >= self.max_peers {
            while peers.front().is_some_and(|e| self.expired(e, now)) {
                peers.pop_front();
            }
        }
        if peers.len() >= self.max_peers {
            return false;
        }
        peers.push_back(Entry {
            peer: peer.to_owned(),
            window_start: now,
            failures: 1,
        });
        true
    }

    pub fn window(&self) -> Duration {
        self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // http-server.test.js 'rejects a missing or malformed API token'
    #[test]
    fn token_validation() {
        let e = ApiToken::parse(None).expect_err("missing");
        assert!(e.0.contains("HAPPY_MCP_API_TOKEN is required"));
        assert!(e.0.contains("openssl rand -hex 32"));
        for bad in [
            "short",
            &HEX[..63],
            &format!("{HEX}0"),
            &"g".repeat(64),
            &"A".repeat(42),
        ] {
            let e = ApiToken::parse(Some(bad)).expect_err(bad);
            assert!(e.0.contains("64 hex or 43 base64url"), "{bad}");
            assert!(!e.0.contains(bad), "token echoed");
        }
        ApiToken::parse(Some(HEX)).expect("hex");
        ApiToken::parse(Some(&"A".repeat(43))).expect("b64url");
        // Non-canonical trailing bits.
        ApiToken::parse(Some(&format!("{}B", "A".repeat(42)))).expect_err("noncanonical");
        assert_eq!(
            format!("{:?}", ApiToken::parse(Some(HEX)).expect("ok")),
            "ApiToken(<redacted>)"
        );
    }

    fn auth(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(header::AUTHORIZATION, HeaderValue::from_str(v).expect("hv"));
        }
        h
    }

    // 'rejects %s credentials'
    #[test]
    fn bearer_matching() {
        let t = ApiToken::parse(Some(HEX)).expect("ok");
        assert!(t.matches(&auth(&[&format!("Bearer {HEX}")])));
        assert!(t.matches(&auth(&[&format!("bearer {HEX}")])));
        for bad in [
            String::new(),
            HEX.to_string(),
            format!("Basic {HEX}"),
            format!("Bearer {}", "f".repeat(64)),
            format!("Bearer  {HEX}"),
            format!("Bearer {HEX} "),
        ] {
            assert!(!t.matches(&auth(&[&bad])), "{bad}");
        }
        assert!(!t.matches(&auth(&[])));
        let one = format!("Bearer {HEX}");
        assert!(!t.matches(&auth(&[&one, &one])));
    }

    // 'tracks failures per peer…', 'bounds tracked peers…'
    #[test]
    fn limiter_windows_and_bounds() {
        let l = AuthFailureLimiter::new(2, Duration::from_millis(1000), 2);
        let t0 = Instant::now();
        assert_eq!(l.retry_after_seconds("a", t0), None);
        assert!(l.record_failure("a", t0));
        assert_eq!(l.retry_after_seconds("a", t0), None);
        assert!(l.record_failure("a", t0));
        assert_eq!(
            l.retry_after_seconds("a", t0 + Duration::from_millis(100)),
            Some(1)
        );
        assert_eq!(l.retry_after_seconds("b", t0), None);
        assert!(l.record_failure("b", t0));
        assert!(!l.record_failure("c", t0), "table full of live windows");
        let later = t0 + Duration::from_millis(1000);
        assert_eq!(l.retry_after_seconds("a", later), None);
        assert!(l.record_failure("c", later), "expired peers pruned");
        assert!(l.size() <= 2);
    }
}
