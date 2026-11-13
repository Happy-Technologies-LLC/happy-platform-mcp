//! OAuth 2.0 authorization_code + PKCE with a loopback redirect (ports
//! `src/oauth-authorization-code.js`). The browser opener and the token POST
//! are injected; tests never launch a browser.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hpm_types::BoxFut;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use url::Url;

use super::error::{ClientError, RawError};
use super::http::{HttpRequest, HttpTransport, Method, form_encode};

/// Token-endpoint POST boundary: form-encodes `params`, never follows a
/// redirect, returns the parsed body.
pub trait TokenPoster: Send + Sync {
    fn post<'a>(
        &'a self,
        url: &'a str,
        params: Vec<(String, String)>,
    ) -> BoxFut<'a, Result<Value, RawError>>;
}

pub struct HttpTokenPoster {
    transport: Arc<dyn HttpTransport>,
}

impl HttpTokenPoster {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }
}

impl TokenPoster for HttpTokenPoster {
    fn post<'a>(
        &'a self,
        url: &'a str,
        params: Vec<(String, String)>,
    ) -> BoxFut<'a, Result<Value, RawError>> {
        Box::pin(async move {
            let pairs: Vec<(&str, &str)> = params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let mut request = HttpRequest::new(Method::Post, url);
            request.set_header("Content-Type", "application/x-www-form-urlencoded");
            request.set_header("Accept", "application/json, text/plain, */*");
            request.body = Some(form_encode(&pairs).into_bytes());
            let response = self.transport.send(request).await.map_err(|e| RawError {
                message: e.message,
                code: e.code,
                ..RawError::default()
            })?;
            if (200..300).contains(&response.status) {
                Ok(response.data())
            } else {
                Err(RawError::http(response.status, response.data()))
            }
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PkcePair {
    pub code_verifier: String,
    pub code_challenge: String,
    pub code_challenge_method: &'static str,
}

impl std::fmt::Debug for PkcePair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PkcePair")
            .field("code_verifier", &"<redacted>")
            .field("code_challenge", &self.code_challenge)
            .field("code_challenge_method", &self.code_challenge_method)
            .finish()
    }
}

impl Drop for PkcePair {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.code_verifier);
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    // OS RNG failure is unrecoverable for PKCE; never fall back to weak randomness.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    buf
}

/// RFC 7636 S256 pair: 32 random bytes as a 43-char base64url verifier.
pub fn generate_pkce_pair() -> PkcePair {
    let code_verifier = URL_SAFE_NO_PAD.encode(random_bytes::<32>());
    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    PkcePair {
        code_verifier,
        code_challenge,
        code_challenge_method: "S256",
    }
}

pub struct AuthorizationRequest<'a> {
    pub authorize_url: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub state: &'a str,
    pub scope: Option<&'a str>,
}

/// `buildAuthorizationUrl` (`URL.searchParams.set` semantics).
pub fn build_authorization_url(req: &AuthorizationRequest<'_>) -> Result<String, ClientError> {
    let mut url = Url::parse(req.authorize_url).map_err(|_| ClientError::plain("Invalid URL"))?;
    let mut set: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", req.client_id),
        ("redirect_uri", req.redirect_uri),
        ("code_challenge", req.code_challenge),
        ("code_challenge_method", "S256"),
        ("state", req.state),
    ];
    if let Some(scope) = req.scope.filter(|s| !s.is_empty()) {
        set.push(("scope", scope));
    }
    let existing: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !set.iter().any(|(n, _)| n == k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut q = url.query_pairs_mut();
        q.clear();
        for (k, v) in &existing {
            q.append_pair(k, v);
        }
        for (k, v) in &set {
            q.append_pair(k, v);
        }
    }
    Ok(url.to_string())
}

fn oauth_error(code: &str, message: impl Into<String>) -> ClientError {
    ClientError::new("Error", message, Some(code))
}

type Settled = Option<Result<String, ClientError>>;

/// Loopback redirect catcher bound to 127.0.0.1.
pub struct CallbackServer {
    port: u16,
    state: watch::Sender<Settled>,
    task: tokio::task::JoinHandle<()>,
}

fn settle(tx: &watch::Sender<Settled>, value: Result<String, ClientError>) -> bool {
    let mut value = Some(value);
    tx.send_if_modified(|current| {
        if current.is_none() {
            *current = value.take();
            true
        } else {
            false
        }
    })
}

fn state_matches(received: &str, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let a = Sha256::digest(received.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn page(status: u16, message: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let body = format!(
        "<!doctype html><html><body style=\"font-family:system-ui;padding:2rem\"><p>{message}</p></body></html>"
    );
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Decides the response for one request line and settles the flow as needed.
fn handle_request(
    request_line: &str,
    callback_path: &str,
    expected_state: &str,
    tx: &watch::Sender<Settled>,
) -> Vec<u8> {
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return page(400, "Bad request.");
    };
    if method != "GET" || url.path() != callback_path {
        return page(404, "Not found.");
    }
    let params: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let all = |name: &str| {
        params
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
    };
    let states = all("state");
    if states.len() != 1 || !state_matches(states[0], expected_state) {
        return page(400, "Invalid sign-in request.");
    }
    if tx.borrow().is_some() {
        return page(409, "This sign-in request has already been handled.");
    }
    if params.iter().any(|(k, _)| k == "error") {
        settle(
            tx,
            Err(oauth_error(
                "OAUTH_AUTHORIZATION_DENIED",
                "OAuth authorization was denied or failed at the provider.",
            )),
        );
        return page(
            400,
            "Authentication was not completed. Return to the terminal for details.",
        );
    }
    let codes = all("code");
    if codes.len() != 1 || codes[0].is_empty() {
        settle(
            tx,
            Err(oauth_error(
                "OAUTH_MISSING_CODE",
                "OAuth callback missing authorization code.",
            )),
        );
        return page(
            400,
            "Authentication failed: no authorization code in the redirect.",
        );
    }
    let code = codes[0].to_owned();
    settle(tx, Ok(code));
    page(
        200,
        "Authentication complete. You can close this tab and return to the terminal.",
    )
}

/// Per-connection bound for reading the redirect request line and headers.
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause after a failed `accept` (e.g. EMFILE) instead of spinning.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

impl CallbackServer {
    /// `createCallbackServer`: binds before returning (`port` 0 = ephemeral).
    pub async fn bind(
        port: u16,
        callback_path: &str,
        expected_state: &str,
    ) -> Result<Self, ClientError> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
            ClientError::new("Error", format!("listen failed: {e}"), Some("EADDRINUSE"))
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| ClientError::plain(e.to_string()))?
            .port();
        let (tx, _) = watch::channel::<Settled>(None);
        let task_tx = tx.clone();
        let path: Arc<str> = Arc::from(callback_path);
        let expected: Arc<str> = Arc::from(expected_state);
        let task = tokio::spawn(async move {
            // Connections are served concurrently, each bounded by a read
            // timeout, so an idle socket (e.g. a browser preconnect) cannot
            // hold up the real redirect. Aborting this task drops the set and
            // with it every connection task.
            let mut connections = tokio::task::JoinSet::new();
            loop {
                while connections.try_join_next().is_some() {}
                let mut stream = match listener.accept().await {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        continue;
                    }
                };
                let (tx, path, expected) = (task_tx.clone(), path.clone(), expected.clone());
                connections.spawn(async move {
                    let mut buf = Vec::with_capacity(1024);
                    let read = async {
                        let mut chunk = [0u8; 1024];
                        while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                    };
                    if tokio::time::timeout(CALLBACK_READ_TIMEOUT, read)
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let text = String::from_utf8_lossy(&buf);
                    let line = text.lines().next().unwrap_or("");
                    let response = handle_request(line, &path, &expected, &tx);
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Ok(Self {
            port,
            state: tx,
            task,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `waitForCode`. `Some(0)` is an immediate timeout, not "no timeout".
    pub async fn wait_for_code(&self, timeout: Option<Duration>) -> Result<String, ClientError> {
        let mut rx = self.state.subscribe();
        let wait = async {
            loop {
                if let Some(v) = rx.borrow_and_update().clone() {
                    return v;
                }
                if rx.changed().await.is_err() {
                    return Err(oauth_error(
                        "OAUTH_CALLBACK_CLOSED",
                        "OAuth callback server closed before sign-in completed.",
                    ));
                }
            }
        };
        match timeout {
            None => wait.await,
            Some(t) => {
                if let Some(v) = self.state.borrow().clone() {
                    return v;
                }
                if let Ok(v) = tokio::time::timeout(t, wait).await {
                    return v;
                }
                settle(
                    &self.state,
                    Err(oauth_error(
                        "OAUTH_CALLBACK_TIMEOUT",
                        format!(
                            "Timed out waiting for OAuth redirect after {}ms",
                            t.as_millis()
                        ),
                    )),
                );
                self.state.borrow().clone().unwrap_or_else(|| {
                    Err(oauth_error(
                        "OAUTH_CALLBACK_TIMEOUT",
                        "Timed out waiting for OAuth redirect",
                    ))
                })
            }
        }
    }

    /// Settles a pending wait with `OAUTH_CALLBACK_CLOSED` and stops listening.
    /// Idempotent.
    pub fn close(&self) {
        settle(
            &self.state,
            Err(oauth_error(
                "OAUTH_CALLBACK_CLOSED",
                "OAuth callback server closed before sign-in completed.",
            )),
        );
        self.task.abort();
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `exchangeAuthorizationCode`: `client_secret` only for confidential clients.
pub fn exchange_params(
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("grant_type".to_owned(), "authorization_code".to_owned()),
        ("client_id".to_owned(), client_id.to_owned()),
        ("code".to_owned(), code.to_owned()),
        ("code_verifier".to_owned(), code_verifier.to_owned()),
        ("redirect_uri".to_owned(), redirect_uri.to_owned()),
    ];
    if let Some(secret) = client_secret.filter(|s| !s.is_empty()) {
        params.push(("client_secret".to_owned(), secret.to_owned()));
    }
    params
}

#[derive(Clone)]
pub struct AuthCodeFlowConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    pub redirect_port: u16,
    pub callback_path: String,
    pub timeout: Duration,
}

impl std::fmt::Debug for AuthCodeFlowConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthCodeFlowConfig")
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &super::redacted(&self.client_secret))
            .field("scope", &self.scope)
            .field("redirect_port", &self.redirect_port)
            .field("callback_path", &self.callback_path)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Drop for AuthCodeFlowConfig {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.client_secret);
    }
}

impl AuthCodeFlowConfig {
    pub fn new(authorize_url: &str, token_url: &str, client_id: &str) -> Self {
        Self {
            authorize_url: authorize_url.to_owned(),
            token_url: token_url.to_owned(),
            client_id: client_id.to_owned(),
            client_secret: None,
            scope: None,
            redirect_port: 0,
            callback_path: "/callback".to_owned(),
            timeout: Duration::from_millis(300_000),
        }
    }
}

/// Opens the authorization URL for the user.
pub trait BrowserOpener: Send + Sync {
    fn open<'a>(&'a self, auth_url: &'a str) -> BoxFut<'a, ()>;
}

/// The OS opener for `platform` (`std::env::consts::OS` names), or `None` for
/// any URL that is not http(s). Never a shell: one argv entry carries the URL.
pub fn opener_command(platform: &str, auth_url: &str) -> Option<(&'static str, Vec<String>)> {
    let scheme = Url::parse(auth_url).ok()?.scheme().to_owned();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    Some(match platform {
        "macos" | "darwin" => ("open", vec![auth_url.to_owned()]),
        "windows" | "win32" => (
            "rundll32.exe",
            vec![
                "url.dll,FileProtocolHandler".to_owned(),
                auth_url.to_owned(),
            ],
        ),
        _ => ("xdg-open", vec![auth_url.to_owned()]),
    })
}

/// Production opener: prints the URL to stderr, then best-effort launches the
/// OS handler without a shell.
pub struct SystemBrowserOpener;

impl BrowserOpener for SystemBrowserOpener {
    fn open<'a>(&'a self, auth_url: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move {
            eprintln!("\nOpen this URL to sign in:\n  {auth_url}\n");
            if let Some((cmd, args)) = opener_command(std::env::consts::OS, auth_url) {
                let _ = std::process::Command::new(cmd)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
        })
    }
}

/// The interactive sign-in seam the client calls (`performAuthCodeFlow`).
pub trait AuthCodeFlow: Send + Sync {
    fn run<'a>(
        &'a self,
        config: AuthCodeFlowConfig,
        post: Arc<dyn TokenPoster>,
    ) -> BoxFut<'a, Result<Value, RawError>>;
}

/// `performAuthorizationCodeFlow`.
pub struct LoopbackAuthCodeFlow {
    opener: Arc<dyn BrowserOpener>,
}

impl LoopbackAuthCodeFlow {
    pub fn new(opener: Arc<dyn BrowserOpener>) -> Self {
        Self { opener }
    }
}

impl AuthCodeFlow for LoopbackAuthCodeFlow {
    fn run<'a>(
        &'a self,
        config: AuthCodeFlowConfig,
        post: Arc<dyn TokenPoster>,
    ) -> BoxFut<'a, Result<Value, RawError>> {
        Box::pin(async move {
            let pkce = generate_pkce_pair();
            let state = URL_SAFE_NO_PAD.encode(random_bytes::<16>());
            let server = CallbackServer::bind(config.redirect_port, &config.callback_path, &state)
                .await
                .map_err(|e| RawError::from_client(&e))?;
            let result = async {
                let redirect_uri =
                    format!("http://127.0.0.1:{}{}", server.port(), config.callback_path);
                let auth_url = build_authorization_url(&AuthorizationRequest {
                    authorize_url: &config.authorize_url,
                    client_id: &config.client_id,
                    redirect_uri: &redirect_uri,
                    code_challenge: &pkce.code_challenge,
                    state: &state,
                    scope: config.scope.as_deref(),
                })
                .map_err(|e| RawError::from_client(&e))?;
                self.opener.open(&auth_url).await;
                let code = server
                    .wait_for_code(Some(config.timeout))
                    .await
                    .map_err(|e| RawError::from_client(&e))?;
                let params = exchange_params(
                    &config.client_id,
                    config.client_secret.as_deref(),
                    &code,
                    &pkce.code_verifier,
                    &redirect_uri,
                );
                // The token endpoint can echo the authorization code (and
                // the verifier) back in its error body: scrub both here.
                let mut scrub = super::error::Redactor::new();
                scrub.add(&code);
                scrub.add(&pkce.code_verifier);
                post.post(&config.token_url, params).await.map_err(|mut e| {
                    e.message = scrub.redact(&e.message);
                    e.data = e.data.map(|d| scrub.sanitize(&d));
                    e
                })
            }
            .await;
            server.close();
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    async fn get(port: u16, target: &str) -> (u16, String) {
        let mut stream = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => s,
            Err(e) => panic!("connect: {e}"),
        };
        let req = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.ok();
        let mut out = String::new();
        stream.read_to_string(&mut out).await.ok();
        let status = out
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = out.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
        (status, body)
    }

    async fn server() -> CallbackServer {
        match CallbackServer::bind(0, "/callback", "good-state").await {
            Ok(s) => s,
            Err(e) => panic!("bind: {e}"),
        }
    }

    fn is_pending(s: &CallbackServer) -> bool {
        s.state.borrow().is_none()
    }

    #[test]
    fn pkce_challenge_is_base64url_sha256_of_verifier() {
        let p = generate_pkce_pair();
        assert_eq!(p.code_challenge_method, "S256");
        assert_eq!(
            p.code_challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(p.code_verifier.as_bytes()))
        );
        assert!((43..=128).contains(&p.code_verifier.len()));
        assert!(
            p.code_verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        );
        assert_ne!(
            generate_pkce_pair().code_verifier,
            generate_pkce_pair().code_verifier
        );
    }

    fn base_req(scope: Option<&str>) -> String {
        build_authorization_url(&AuthorizationRequest {
            authorize_url: "https://example.service-now.com/oauth_auth.do",
            client_id: "cli-client-id",
            redirect_uri: "http://127.0.0.1:8455/callback",
            code_challenge: "abc123challenge",
            state: "xyz-state",
            scope,
        })
        .unwrap_or_default()
    }

    #[test]
    fn authorization_url_params() {
        let url = Url::parse(&base_req(None)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            format!("{}{}", url.origin().ascii_serialization(), url.path()),
            "https://example.service-now.com/oauth_auth.do"
        );
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "cli-client-id");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:8455/callback");
        assert_eq!(q["code_challenge"], "abc123challenge");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "xyz-state");
        assert!(!q.contains_key("scope"));
        let with = Url::parse(&base_req(Some("useraccount"))).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            with.query_pairs()
                .any(|(k, v)| k == "scope" && v == "useraccount")
        );
    }

    #[tokio::test]
    async fn resolves_with_code_on_matching_state() {
        let s = server().await;
        let (status, _) = get(s.port(), "/callback?code=THE_CODE&state=good-state").await;
        assert_eq!(status, 200);
        assert_eq!(
            s.wait_for_code(None).await.ok().as_deref(),
            Some("THE_CODE")
        );
    }

    #[tokio::test]
    async fn foreign_state_is_ignored_then_valid_accepted() {
        let s = server().await;
        assert_eq!(
            get(s.port(), "/callback?code=EVIL&state=tampered").await.0,
            400
        );
        assert!(is_pending(&s));
        get(s.port(), "/callback?code=THE_CODE&state=good-state").await;
        assert_eq!(
            s.wait_for_code(None).await.ok().as_deref(),
            Some("THE_CODE")
        );
    }

    #[tokio::test]
    async fn provider_error_rejects_with_fixed_code_never_reflecting_text() {
        let s = server().await;
        let payload = "%3Cscript%3Ealert(1)%3C%2Fscript%3E%20ignore%20previous%20instructions";
        let (status, body) = get(
            s.port(),
            &format!("/callback?error={payload}&error_description={payload}&state=good-state"),
        )
        .await;
        assert_eq!(status, 400);
        assert!(
            !body.to_lowercase().contains("script")
                && !body.contains("alert")
                && !body.contains("ignore previous")
        );
        let err = s
            .wait_for_code(None)
            .await
            .err()
            .unwrap_or_else(|| ClientError::plain(""));
        assert_eq!(err.code(), Some("OAUTH_AUTHORIZATION_DENIED"));
        assert!(!err.message.contains("access_denied") && !err.message.contains("alert"));
    }

    #[tokio::test]
    async fn valid_state_without_code_rejects() {
        let s = server().await;
        get(s.port(), "/callback?state=good-state").await;
        let err = s
            .wait_for_code(None)
            .await
            .err()
            .map(|e| e.message)
            .unwrap_or_default();
        assert!(err.to_lowercase().contains("code"));
    }

    #[tokio::test]
    async fn wrong_missing_or_duplicate_state_does_not_settle() {
        let s = server().await;
        for target in [
            "/callback?error=access_denied&state=bad",
            "/callback?error=%3Cb%3Ex%3C%2Fb%3E",
            "/callback?code=C&state=good-state&state=good-state",
        ] {
            let (status, body) = get(s.port(), target).await;
            assert_eq!(status, 400);
            assert!(!body.contains("access_denied") && !body.contains("<b>"));
        }
        assert!(is_pending(&s));
        get(s.port(), "/callback?code=OK&state=good-state").await;
        assert_eq!(s.wait_for_code(None).await.ok().as_deref(), Some("OK"));
    }

    #[tokio::test]
    async fn unrelated_paths_are_404_without_settling() {
        let s = server().await;
        assert_eq!(get(s.port(), "/favicon.ico").await.0, 404);
        assert_eq!(get(s.port(), "/other?state=good-state&code=X").await.0, 404);
        get(s.port(), "/callback?code=REAL&state=good-state").await;
        assert_eq!(s.wait_for_code(None).await.ok().as_deref(), Some("REAL"));
    }

    #[tokio::test]
    async fn settles_only_once() {
        let s = server().await;
        assert_eq!(
            get(s.port(), "/callback?code=FIRST&state=good-state")
                .await
                .0,
            200
        );
        assert_eq!(
            get(s.port(), "/callback?code=SECOND&state=good-state")
                .await
                .0,
            409
        );
        assert_eq!(
            get(s.port(), "/callback?error=access_denied&state=good-state")
                .await
                .0,
            409
        );
        assert_eq!(s.wait_for_code(None).await.ok().as_deref(), Some("FIRST"));
        assert_eq!(s.wait_for_code(None).await.ok().as_deref(), Some("FIRST"));
    }

    #[tokio::test]
    async fn timeout_settles_and_later_callbacks_are_refused() {
        let s = server().await;
        let e = s
            .wait_for_code(Some(Duration::from_millis(10)))
            .await
            .err()
            .map(|e| e.message)
            .unwrap_or_default();
        assert!(e.to_lowercase().contains("timed out"));
        assert_eq!(
            get(s.port(), "/callback?code=LATE&state=good-state")
                .await
                .0,
            409
        );
        let again = s
            .wait_for_code(None)
            .await
            .err()
            .map(|e| e.message)
            .unwrap_or_default();
        assert!(again.to_lowercase().contains("timed out"));
    }

    #[tokio::test]
    async fn close_rejects_pending_and_is_idempotent() {
        let s = server().await;
        s.close();
        s.close();
        assert_eq!(
            s.wait_for_code(None).await.err().and_then(|e| e.code),
            Some("OAUTH_CALLBACK_CLOSED".into())
        );
    }

    #[tokio::test]
    async fn zero_timeout_is_immediate() {
        let s = server().await;
        let e = s
            .wait_for_code(Some(Duration::ZERO))
            .await
            .err()
            .map(|e| e.message)
            .unwrap_or_default();
        assert!(e.to_lowercase().contains("timed out"));
    }

    struct CapturePost(Mutex<Vec<(String, Vec<(String, String)>)>>);
    impl TokenPoster for CapturePost {
        fn post<'a>(
            &'a self,
            url: &'a str,
            params: Vec<(String, String)>,
        ) -> BoxFut<'a, Result<Value, RawError>> {
            Box::pin(async move {
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((url.to_owned(), params));
                Ok(serde_json::json!({"access_token":"at","refresh_token":"rt","expires_in":1800}))
            })
        }
    }

    #[test]
    fn exchange_omits_secret_for_public_clients() {
        let p = exchange_params(
            "cli-client-id",
            None,
            "AUTH_CODE",
            "the-verifier",
            "http://127.0.0.1:8455/callback",
        );
        assert!(p.contains(&("grant_type".into(), "authorization_code".into())));
        assert!(p.contains(&("code_verifier".into(), "the-verifier".into())));
        assert!(!p.iter().any(|(k, _)| k == "client_secret"));
        let c = exchange_params("c", Some("shh"), "x", "v", "r");
        assert!(c.contains(&("client_secret".into(), "shh".into())));
    }

    /// Simulated browser: hits the loopback redirect with the issued state.
    struct RedirectingOpener;
    impl BrowserOpener for RedirectingOpener {
        fn open<'a>(&'a self, auth_url: &'a str) -> BoxFut<'a, ()> {
            Box::pin(async move {
                let url = Url::parse(auth_url).unwrap_or_else(|e| panic!("{e}"));
                let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
                let redirect = Url::parse(&q["redirect_uri"]).unwrap_or_else(|e| panic!("{e}"));
                let port = redirect.port().unwrap_or(0);
                let target = format!("{}?code=AUTH_CODE&state={}", redirect.path(), q["state"]);
                tokio::spawn(async move { get(port, &target).await });
            })
        }
    }

    #[tokio::test]
    async fn full_flow_end_to_end() {
        let post = Arc::new(CapturePost(Mutex::new(Vec::new())));
        let flow = LoopbackAuthCodeFlow::new(Arc::new(RedirectingOpener));
        let mut cfg = AuthCodeFlowConfig::new(
            "https://example.service-now.com/oauth_auth.do",
            "https://example.service-now.com/oauth_token.do",
            "cli-client-id",
        );
        cfg.scope = Some("useraccount".into());
        let tokens = flow.run(cfg, post.clone()).await.ok().unwrap_or_default();
        assert_eq!(tokens["access_token"], "at");
        let calls = post.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let params: std::collections::HashMap<_, _> = calls[0].1.iter().cloned().collect();
        assert_eq!(params["grant_type"], "authorization_code");
        assert_eq!(params["code"], "AUTH_CODE");
        assert!(!params["code_verifier"].is_empty());
        let r = &params["redirect_uri"];
        assert!(r.starts_with("http://127.0.0.1:") && r.ends_with("/callback"));
    }

    /// oauth-default-opener.test.js
    #[test]
    fn default_opener_commands() {
        let url =
            "https://dev.service-now.com/oauth_auth.do?response_type=code&client_id=cli&state=s";
        let (cmd, args) = opener_command("windows", url).unwrap_or(("", vec![]));
        assert_eq!(cmd, "rundll32.exe");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "url.dll,FileProtocolHandler");
        assert_eq!(args[1], url);
        assert_eq!(opener_command("macos", url).map(|c| c.0), Some("open"));
        assert_eq!(opener_command("linux", url).map(|c| c.0), Some("xdg-open"));
        for bad in [
            "file:///C:/Windows/System32/calc.exe",
            "javascript:alert(1)//x",
        ] {
            assert!(opener_command("windows", bad).is_none());
        }
    }

    struct NoopOpener;
    impl BrowserOpener for NoopOpener {
        fn open<'a>(&'a self, _: &'a str) -> BoxFut<'a, ()> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn flow_times_out_with_callback_timeout_code() {
        let post = Arc::new(CapturePost(Mutex::new(Vec::new())));
        let mut cfg = AuthCodeFlowConfig::new(
            "https://dev.service-now.com/oauth_auth.do",
            "https://idp.example/oauth_token.do",
            "cli",
        );
        cfg.timeout = Duration::from_millis(50);
        let err = LoopbackAuthCodeFlow::new(Arc::new(NoopOpener))
            .run(cfg, post)
            .await
            .err()
            .unwrap_or_default();
        assert_eq!(err.code.as_deref(), Some("OAUTH_CALLBACK_TIMEOUT"));
    }
}
