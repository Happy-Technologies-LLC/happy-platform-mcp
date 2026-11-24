//! T-K02: HTTP streams end at the opening credential's expiry unless the same
//! principal refreshes it on the session. In-process router, paused clock.

use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Request, Response, StatusCode, header};
use hpm_protocol::McpHandler;
use hpm_protocol::transport::limits::HttpSecurity;
use hpm_protocol::transport::{
    AuthChallenge, AuthRequest, HttpApp, HttpAuthenticator, HttpConfig, PeerAddrs,
};
use hpm_tools::dispatcher::{Dispatcher, RequiredPropertiesValidator, Services};
use hpm_tools::registration::ToolRegistry;
use hpm_tools::single_user::{
    AllowAll, LocalOnlyCredentials, PassThrough, StderrAudit, UnloadedClassification,
};
use hpm_types::BoxFut;
use hpm_types::context::{
    AdmissionKey, ClientIdentity, DeploymentMode, Principal, TrustedRoot, VerifiedHttpIdentity,
};
use hpm_types::operation::ScriptOperandGuard;
use serde_json::{Value, json};
use tower::Service;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const LOCAL: &str = "127.0.0.1:3000";
const LEGACY: &str = "2024-11-05";
const SESSION: &str = "2025-11-25";

static KEY: LazyLock<AdmissionKey> =
    LazyLock::new(|| AdmissionKey::mint().expect("first mint in this process"));

/// `Bearer <sub>:<ttl seconds>`; `Bearer <sub>:none` has no expiry.
struct Ttl;

impl HttpAuthenticator for Ttl {
    fn authenticate<'a>(
        &'a self,
        req: AuthRequest<'a>,
    ) -> BoxFut<'a, Result<VerifiedHttpIdentity, AuthChallenge>> {
        let token = req
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string);
        Box::pin(async move {
            let Some((sub, ttl)) = token.as_deref().and_then(|t| t.split_once(':')) else {
                return Err(AuthChallenge::unauthorized(None));
            };
            let id = VerifiedHttpIdentity::new(
                Principal::Federated {
                    org: "acme".into(),
                    iss: "https://idp.example".into(),
                    sub: sub.into(),
                },
                ClientIdentity {
                    client_id: "c".into(),
                    verified: true,
                },
                Vec::new().into(),
            );
            Ok(match ttl.parse::<u64>() {
                Ok(s) => id.with_expires_at(SystemTime::now() + Duration::from_secs(s)),
                Err(_) => id,
            })
        })
    }
}

fn dispatcher() -> Arc<Dispatcher> {
    let services = Services {
        validator: Arc::new(RequiredPropertiesValidator),
        classification: Arc::new(UnloadedClassification),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: Arc::new(LocalOnlyCredentials),
        audit: Arc::new(StderrAudit),
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
        usage: None,
    };
    Arc::new(Dispatcher::new(
        ToolRegistry::new().expect("registry"),
        services,
        *KEY,
    ))
}

fn config() -> HttpConfig {
    let mut config = HttpConfig::new(
        HttpSecurity::new(TOKEN, &[], &[]).expect("security"),
        Arc::new(|| json!({})),
        Arc::new(|| Box::pin(async { Ok(json!([])) })),
    );
    // Only the credential deadline may end a stream in these tests.
    let day = Duration::from_secs(24 * 3600);
    config.limits.session_setup_timeout = day;
    config.limits.session_idle_timeout = day;
    config.limits.session_max_lifetime = day;
    // rmcp's keepalive spins under a paused clock; keepalives are not under test.
    config.keepalive = day;
    config
}

fn federated_app() -> Router {
    let d = dispatcher();
    HttpApp::with_authenticator(
        McpHandler::new(
            TrustedRoot::streamable_http(DeploymentMode::MultiUser),
            d.clone(),
        ),
        McpHandler::new(TrustedRoot::legacy_sse(DeploymentMode::MultiUser), d),
        config(),
        Arc::new(Ttl),
    )
    .router()
}

fn single_user_app() -> Router {
    let d = dispatcher();
    HttpApp::new(
        McpHandler::new(
            TrustedRoot::streamable_http(DeploymentMode::SingleUser),
            d.clone(),
        ),
        McpHandler::new(TrustedRoot::legacy_sse(DeploymentMode::SingleUser), d),
        config(),
    )
    .router()
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    auth: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Response<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, LOCAL)
        .header(header::AUTHORIZATION, format!("Bearer {auth}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let mut req = req
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .expect("request");
    let local: SocketAddr = LOCAL.parse().expect("addr");
    req.extensions_mut().insert(ConnectInfo(PeerAddrs {
        local,
        remote: "127.0.0.1:50000".parse().expect("addr"),
    }));
    app.clone().call(req).await.expect("infallible")
}

/// Reads frames until `needle` appears; panics if the stream ends first.
async fn read_until(body: &mut Body, needle: &str) -> String {
    let mut seen = String::new();
    while !seen.contains(needle) {
        let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx))
            .await
            .expect("stream ended early")
            .expect("frame");
        if let Ok(data) = frame.into_data() {
            seen.push_str(&String::from_utf8_lossy(&data));
        }
    }
    seen
}

/// `true` when the stream ends cleanly within `within` (frames are drained).
async fn ends_within(body: &mut Body, within: Duration) -> bool {
    tokio::time::timeout(within, async {
        loop {
            match std::future::poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx)).await {
                None => return true,
                Some(Err(_)) => return false,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .unwrap_or(false)
}

fn initialize(version: &str) -> Value {
    json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
        "protocolVersion": version, "capabilities": {}, "clientInfo": {"name":"t","version":"1"}}})
}

async fn open_legacy(app: &Router, auth: &str) -> (Body, String) {
    let r = send(
        app,
        "GET",
        "/mcp",
        auth,
        &[("Accept", "text/event-stream")],
        None,
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body();
    let seen = read_until(&mut body, "sessionId=").await;
    let tail = &seen[seen.find("sessionId=").expect("id") + "sessionId=".len()..];
    let sid = tail.split(['\r', '\n']).next().expect("id").to_string();
    (body, sid)
}

async fn legacy_post(app: &Router, sid: &str, auth: &str) -> StatusCode {
    send(
        app,
        "POST",
        &format!("/mcp?sessionId={sid}"),
        auth,
        &[],
        Some(&initialize(LEGACY)),
    )
    .await
    .status()
}

#[tokio::test(start_paused = true)]
async fn legacy_stream_ends_at_expiry() {
    let app = federated_app();
    let (mut body, _) = open_legacy(&app, "alice:60").await;
    assert!(!ends_within(&mut body, Duration::from_secs(59)).await);
    assert!(ends_within(&mut body, Duration::from_secs(2)).await);
}

#[tokio::test(start_paused = true)]
async fn legacy_refresh_by_same_principal_extends() {
    let app = federated_app();
    let (mut body, sid) = open_legacy(&app, "alice:60").await;
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(
        legacy_post(&app, &sid, "alice:120").await,
        StatusCode::ACCEPTED
    );
    // Past the opening token's expiry (t=60), before the refreshed one (t=150).
    assert!(!ends_within(&mut body, Duration::from_secs(110)).await);
    assert!(ends_within(&mut body, Duration::from_secs(20)).await);
}

#[tokio::test(start_paused = true)]
async fn legacy_other_principal_cannot_extend() {
    let app = federated_app();
    let (mut body, sid) = open_legacy(&app, "alice:60").await;
    assert_eq!(
        legacy_post(&app, &sid, "bob:3600").await,
        StatusCode::NOT_FOUND
    );
    assert!(ends_within(&mut body, Duration::from_secs(61)).await);
}

#[tokio::test(start_paused = true)]
async fn single_user_stream_has_no_deadline() {
    let app = single_user_app();
    let (mut body, _) = open_legacy(&app, TOKEN).await;
    // Hours of keepalives: never ended by a credential deadline.
    assert!(!ends_within(&mut body, Duration::from_secs(4 * 3600)).await);
}

async fn streamable_session(app: &Router, auth: &str) -> String {
    let r = send(
        app,
        "POST",
        "/mcp",
        auth,
        &[("Accept", "application/json, text/event-stream")],
        Some(&initialize(SESSION)),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let sid = r
        .headers()
        .get("mcp-session-id")
        .and_then(|v: &HeaderValue| v.to_str().ok())
        .expect("session id")
        .to_string();
    let mut body = r.into_body();
    read_until(&mut body, "serverInfo").await;
    sid
}

async fn streamable_notify(app: &Router, sid: &str, auth: &str) -> StatusCode {
    send(
        app,
        "POST",
        "/mcp",
        auth,
        &[
            ("Accept", "application/json, text/event-stream"),
            ("mcp-session-id", sid),
            ("mcp-protocol-version", SESSION),
        ],
        Some(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})),
    )
    .await
    .status()
}

async fn streamable_get(app: &Router, sid: &str, auth: &str) -> Body {
    let r = send(
        app,
        "GET",
        "/mcp",
        auth,
        &[
            ("Accept", "text/event-stream"),
            ("mcp-session-id", sid),
            ("mcp-protocol-version", SESSION),
        ],
        None,
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    r.into_body()
}

#[tokio::test(start_paused = true)]
async fn streamable_stream_ends_at_expiry_and_refreshes() {
    let app = federated_app();
    let sid = streamable_session(&app, "alice:60").await;
    assert_eq!(
        streamable_notify(&app, &sid, "alice:60").await,
        StatusCode::ACCEPTED
    );
    let mut stream = streamable_get(&app, &sid, "alice:60").await;
    assert!(!ends_within(&mut stream, Duration::from_secs(30)).await);
    // Another principal is refused and does not extend the deadline.
    assert_eq!(
        streamable_notify(&app, &sid, "bob:3600").await,
        StatusCode::NOT_FOUND
    );
    assert!(ends_within(&mut stream, Duration::from_secs(31)).await);
    drop(stream);

    // A newer token for the same principal extends the open stream.
    let mut stream = streamable_get(&app, &sid, "alice:60").await;
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(
        streamable_notify(&app, &sid, "alice:120").await,
        StatusCode::ACCEPTED
    );
    assert!(!ends_within(&mut stream, Duration::from_secs(90)).await);
    assert!(ends_within(&mut stream, Duration::from_secs(31)).await);
}
