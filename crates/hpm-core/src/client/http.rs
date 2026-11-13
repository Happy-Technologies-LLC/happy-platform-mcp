//! Outbound HTTP seam. Every ServiceNow and token-endpoint request goes through
//! [`HttpTransport`], so tests run against in-process fakes and the production
//! [`ReqwestTransport`] never follows a redirect unless a request asks for it.

use std::time::Duration;

use hpm_types::BoxFut;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    /// Absolute URL.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    /// 0 = never follow a redirect (3xx is returned to the caller).
    pub max_redirects: u8,
}

/// Header values and the body may carry credentials (Authorization, Cookie,
/// token-exchange form): `Debug` shows header names and the body length only.
impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &names)
            .field("body_len", &self.body.as_ref().map(Vec::len))
            .field("timeout", &self.timeout)
            .field("max_redirects", &self.max_redirects)
            .finish()
    }
}

impl HttpRequest {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
            max_redirects: 0,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Sets (replaces) a header.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.headers.push((name.to_owned(), value.into()));
    }

    pub fn body_text(&self) -> &str {
        self.body
            .as_deref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }
}

#[derive(Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Set-Cookie values and token-endpoint bodies are credentials: `Debug`
/// shows the status, header names and body length only.
impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &names)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::new(status, value.to_string()).with_header("content-type", "application/json")
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn header_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Axios-style body: parsed JSON when it parses, else the raw text
    /// (`Null` for an empty body).
    pub fn data(&self) -> serde_json::Value {
        if self.body.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| serde_json::Value::String(self.text()))
    }
}

/// A failure before any HTTP status was received. `code` follows the Node
/// error codes the 6.x classifiers use (`ECONNREFUSED`, `ETIMEDOUT`, ...).
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    pub code: Option<String>,
    pub message: String,
    /// The connection (DNS, TCP or TLS) could not be established, so no byte
    /// of the request was sent, whatever `code` says (a connect-phase timeout
    /// keeps `ETIMEDOUT`). Live smoke D1.
    pub connect: bool,
}

impl TransportError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: Some(code.to_owned()),
            message: message.into(),
            connect: false,
        }
    }

    /// A failure while connecting: nothing was sent (see [`Self::connect`]).
    pub fn connect(code: &str, message: impl Into<String>) -> Self {
        Self {
            connect: true,
            ..Self::new(code, message)
        }
    }
}

pub trait HttpTransport: Send + Sync {
    fn send(&self, request: HttpRequest) -> BoxFut<'_, Result<HttpResponse, TransportError>>;
}

/// Production transport. No cookie store (callers manage session cookies
/// explicitly), no proxies beyond reqwest's defaults.
pub struct ReqwestTransport {
    no_redirect: reqwest::Client,
    follow: reqwest::Client,
}

/// TCP/TLS connect bound for every request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Whole-request bound when the caller sets none (axios had none; deviation).
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest response body read into memory.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Followed redirects (max 5) must keep scheme, host and port: credentials
/// set by hand (Authorization, Cookie) never leave the origin or downgrade
/// https→http.
fn same_origin_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let Some(first) = attempt.previous().first() else {
            return attempt.stop();
        };
        if attempt.previous().len() > 5 {
            return attempt.error("too many redirects");
        }
        if attempt.url().origin() == first.origin() {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, TransportError> {
        let build = |policy| {
            reqwest::Client::builder()
                .redirect(policy)
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(DEFAULT_REQUEST_TIMEOUT)
                .build()
                .map_err(|e| TransportError::new("ERR_TRANSPORT_INIT", e.to_string()))
        };
        Ok(Self {
            no_redirect: build(reqwest::redirect::Policy::none())?,
            follow: build(same_origin_policy())?,
        })
    }
}

fn map_reqwest_error(error: &reqwest::Error) -> TransportError {
    let code = if error.is_timeout() {
        "ETIMEDOUT"
    } else if error.is_connect() {
        "ECONNREFUSED"
    } else if error.is_redirect() {
        "ERR_FR_TOO_MANY_REDIRECTS"
    } else {
        "ECONNRESET"
    };
    // reqwest's Display stops at "error sending request for url (...)"; the
    // cause (DNS, TCP, TLS, timeout) lives in the source chain. 6.x (axios)
    // showed it, e.g. `connect ETIMEDOUT 1.2.3.4:443`, so keep it and the code.
    let mut message = format!("{code}: {error}");
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    if error.is_connect() {
        TransportError::connect(code, message)
    } else {
        TransportError::new(code, message)
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(&self, request: HttpRequest) -> BoxFut<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let client = if request.max_redirects == 0 {
                &self.no_redirect
            } else {
                &self.follow
            };
            let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes())
                .map_err(|e| TransportError::new("ERR_INVALID_METHOD", e.to_string()))?;
            let mut builder = client.request(method, &request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            if let Some(timeout) = request.timeout {
                builder = builder.timeout(timeout);
            }
            let response = builder.send().await.map_err(|e| map_reqwest_error(&e))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_owned(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let mut response = response;
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| map_reqwest_error(&e))? {
                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(TransportError::new(
                        "ERR_RESPONSE_TOO_LARGE",
                        format!("Response body exceeds {MAX_RESPONSE_BYTES} bytes"),
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}

/// Monotonic-enough wall clock plus sleep, injectable for deterministic tests.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
    fn sleep(&self, ms: u64) -> BoxFut<'_, ()>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
    fn sleep(&self, ms: u64) -> BoxFut<'_, ()> {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }
}

/// `application/x-www-form-urlencoded` (WHATWG `URLSearchParams.toString()`).
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// Axios 1.x `params` encoding: `encodeURIComponent`, then `:`, `$`, `,`, `[`,
/// `]` restored and `%20` as `+`.
pub fn axios_param_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')'
            | b':'
            | b'$'
            | b','
            | b'['
            | b']' => out.push(char::from(b)),
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

pub fn axios_query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", axios_param_encode(k), axios_param_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Formats epoch milliseconds as `YYYY-MM-DD HH:MM:SS` in UTC (the
/// `toISOString().slice(0, 19).replace('T', ' ')` of 6.x).
pub fn format_utc_datetime(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// ISO 8601 with milliseconds (`Date.prototype.toISOString`).
pub fn format_iso(ms: i64) -> String {
    let base = format_utc_datetime(ms).replace(' ', "T");
    format!("{base}.{:03}Z", ms.rem_euclid(1000))
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        // 2026-08-28T12:00:01Z
        assert_eq!(
            format_utc_datetime(1_787_918_401_000),
            "2026-08-28 12:00:01"
        );
        assert_eq!(format_iso(1_787_918_401_123), "2026-08-28T12:00:01.123Z");
        assert_eq!(format_utc_datetime(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn encodings() {
        assert_eq!(form_encode(&[("q", "a=b c,*")]), "q=a%3Db+c%2C*");
        assert_eq!(axios_param_encode("csrf_token,id"), "csrf_token,id");
        assert_eq!(axios_param_encode("id=ABC"), "id%3DABC");
        assert_eq!(axios_param_encode("a^b c"), "a%5Eb+c");
    }

    /// Live smoke N1: the tool text must carry the code and the OS cause,
    /// not just reqwest's "error sending request for url (...)".
    #[tokio::test]
    async fn connect_failure_message_keeps_code_and_cause() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let err = ReqwestTransport::new()
            .expect("transport")
            .send(HttpRequest {
                method: Method::Get,
                url: format!("http://127.0.0.1:{port}/api/now/table/sys_user"),
                headers: vec![],
                body: None,
                timeout: Some(Duration::from_secs(5)),
                max_redirects: 0,
            })
            .await
            .expect_err("nothing listens on the port");
        assert_eq!(err.code.as_deref(), Some("ECONNREFUSED"));
        assert!(err.connect, "a refused connection sent nothing");
        assert!(err.message.starts_with("ECONNREFUSED: "), "{}", err.message);
        // The io::Error at the bottom of the chain ("Connection refused (os
        // error 61)" on macOS, 111 on Linux, 10061 on Windows).
        assert!(
            err.message.contains("os error"),
            "cause missing: {}",
            err.message
        );
    }

    /// Live smoke D1: a timeout once the request was sent (the server accepted
    /// and never answered) is not a connect-phase failure.
    #[tokio::test]
    async fn read_timeout_after_send_is_not_a_connect_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept");
            std::future::pending::<()>().await;
            drop(socket);
        });
        let err = ReqwestTransport::new()
            .expect("transport")
            .send(HttpRequest {
                method: Method::Post,
                url: format!("http://127.0.0.1:{port}/sys.scripts.do"),
                headers: vec![],
                body: Some(b"script=x".to_vec()),
                timeout: Some(Duration::from_millis(300)),
                max_redirects: 0,
            })
            .await
            .expect_err("the server never answers");
        server.abort();
        assert_eq!(err.code.as_deref(), Some("ETIMEDOUT"), "{}", err.message);
        assert!(!err.connect, "{}", err.message);
    }
}
