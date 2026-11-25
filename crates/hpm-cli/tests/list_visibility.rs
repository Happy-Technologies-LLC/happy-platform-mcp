//! `tools/list` and `resources/list` through the real `McpHandler` honour
//! `PolicyEngine::visible` (spec §5.5, §9 item 4: a user whose rules allow a tool on
//! no instance does not see it). `get_tool` stays unfiltered; `call_tool` decides.
//! One `AdmissionKey` per process, so every test shares one key.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode, header};
use hpm_protocol::McpHandler;
use hpm_protocol::transport::limits::HttpSecurity;
use hpm_protocol::transport::{
    AuthChallenge, AuthRequest, HttpApp, HttpAuthenticator, HttpConfig, PeerAddrs,
};
use hpm_tools::dispatcher::{Dispatcher, RequiredPropertiesValidator, Services};
use hpm_tools::registration::ToolRegistry;
use hpm_tools::single_user::{AllowAll, LocalOnlyCredentials, PassThrough, StderrAudit};
use hpm_types::BoxFut;
use hpm_types::context::{
    AdmissionKey, ClientIdentity, DeploymentMode, Principal, RequestContext, TrustedRoot,
    VerifiedHttpIdentity,
};
use hpm_types::credential::CredentialNeed;
use hpm_types::operation::{Action, Operation, OperationSet, ScriptOperandGuard, Subject};
use hpm_types::policy::{Decision, PolicyDecision, PolicyEngine, PolicyError};
use hpm_types::tool::{
    AccessClass, HandlerCompletion, NormalizeInput, OutputClass, ResourceDescriptor,
    ResourceHandler, ResultBody, RetryEligibility, SideEffectClass, ToolDescriptor, ToolError,
    ToolHandler,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::Service;

const READ_TOOL: &str = "SN-Query-Table";
const REMOTE_TOOL: &str = "SN-Get-Record";
const WRITE_TOOL: &str = "SN-Update-Record";
const RES_A: &str = "servicenow://instance";
const RES_B: &str = "servicenow://other";

const LOCAL: &str = "127.0.0.1:3000";
const SESSION: &str = "2025-11-25";
const BOTH: &str = "application/json, text/event-stream";
const WITHIN: Duration = Duration::from_secs(5);

static KEY: LazyLock<AdmissionKey> =
    LazyLock::new(|| AdmissionKey::mint().expect("first mint in this process"));

struct Never;
impl ToolHandler for Never {
    fn call<'a>(&'a self, _: &'a RequestContext, args: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move { HandlerCompletion::not_a_mutation(Ok(ResultBody::Json(args))) })
    }
}

impl ResourceHandler for Never {
    fn read<'a>(&'a self, _: &'a RequestContext) -> BoxFut<'a, Result<Value, ToolError>> {
        Box::pin(async { Ok(json!({})) })
    }
}

fn normalize(
    subject: &'static str,
) -> fn(&Value, &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    match subject {
        READ_TOOL => |_, _| {
            Ok(OperationSet(vec![Operation::new(
                Subject::Tool(READ_TOOL),
                Action::Read,
            )]))
        },
        REMOTE_TOOL => |_, _| {
            Ok(OperationSet(vec![Operation::new(
                Subject::Tool(REMOTE_TOOL),
                Action::Read,
            )]))
        },
        _ => |_, i| {
            Ok(OperationSet(vec![
                Operation::new(Subject::Tool(WRITE_TOOL), Action::Update)
                    .on_table("incident", i.classification),
            ]))
        },
    }
}

const SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string"}}}"#;

fn render(body: &ResultBody) -> String {
    format!("{body:?}")
}

fn dispatcher(policy: Arc<dyn PolicyEngine>) -> Arc<Dispatcher> {
    let mut registry = ToolRegistry::new().expect("registry");
    let read = |name: &'static str| {
        ToolDescriptor::new(
            name,
            "echo",
            SCHEMA,
            normalize(name),
            Arc::new(Never),
            render,
        )
        .output(OutputClass::Records)
        .credentials(CredentialNeed::None)
    };
    registry
        .register_tool(read(READ_TOOL).needs_instance(true))
        .expect("register");
    registry.register_tool(read(REMOTE_TOOL)).expect("register");
    registry
        .register_tool(
            ToolDescriptor::new(
                WRITE_TOOL,
                "write",
                SCHEMA,
                normalize(WRITE_TOOL),
                Arc::new(Never),
                render,
            )
            .access(AccessClass::Write)
            .output(OutputClass::Records)
            .side_effect(SideEffectClass::Remote)
            .retry(RetryEligibility::BeforeSendOnly),
        )
        .expect("register");
    for (uri, name) in [(RES_A, "a"), (RES_B, "b")] {
        registry
            .register_resource(ResourceDescriptor::new(
                uri,
                name,
                "res",
                "application/json",
                OutputClass::Records,
                Arc::new(Never),
            ))
            .expect("resource");
    }
    let services = Services {
        validator: Arc::new(RequiredPropertiesValidator),
        classification: Arc::new(hpm_tools::single_user::UnloadedClassification),
        policy,
        protector: Arc::new(PassThrough),
        credentials: Arc::new(LocalOnlyCredentials),
        audit: Arc::new(StderrAudit),
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
        usage: None,
    };
    Arc::new(Dispatcher::new(registry, services, *KEY))
}

/// Enforcing engine whose `visible` depends on the principal's `sub`: `alice` sees
/// everything, `carol` sees `SN-Query-Table` and `servicenow://instance`, anyone
/// else sees nothing. `decide` allows (visibility is never authorization).
struct PerUser;

fn sub_of(ctx: &RequestContext) -> String {
    match ctx.principal() {
        Principal::Federated { sub, .. } => sub.clone(),
        _ => String::new(),
    }
}

impl PolicyEngine for PerUser {
    fn decide<'a>(
        &'a self,
        _: &'a RequestContext,
        _: &'a OperationSet,
    ) -> BoxFut<'a, Result<PolicyDecision, PolicyError>> {
        Box::pin(async {
            Ok(PolicyDecision {
                decision: Decision::Allow,
                policy_version: 1,
            })
        })
    }
    fn visible<'a>(&'a self, ctx: &'a RequestContext, subject: &'a Subject) -> BoxFut<'a, bool> {
        let sub = sub_of(ctx);
        let name = match subject {
            Subject::Tool(n) => (*n).to_owned(),
            Subject::Resource(u) => u.clone(),
            _ => String::new(),
        };
        Box::pin(async move {
            match sub.as_str() {
                "alice" => true,
                "carol" => name == READ_TOOL || name == RES_A,
                _ => false,
            }
        })
    }
    fn enforces(&self) -> bool {
        true
    }
}

// ------------------------------------------------------------------ stdio

struct Stdio {
    tx: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
}

fn initialize() -> Value {
    json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
        "protocolVersion": SESSION, "capabilities": {}, "clientInfo": {"name":"t","version":"1"}}})
}

async fn stdio(policy: Arc<dyn PolicyEngine>) -> Stdio {
    let handler = McpHandler::builder(TrustedRoot::single_user_stdio(), dispatcher(policy)).build();
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (server_read, server_write) = tokio::io::split(server_io);
    tokio::spawn(hpm_protocol::stdio::serve_io(
        handler,
        server_read,
        server_write,
    ));
    let (client_read, mut tx) = tokio::io::split(client_io);
    let mut lines = BufReader::new(client_read).lines();
    tx.write_all(format!("{}\n", initialize()).as_bytes())
        .await
        .expect("write");
    tokio::time::timeout(WITHIN, lines.next_line())
        .await
        .expect("init in time")
        .expect("read")
        .expect("line");
    tx.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .expect("write");
    Stdio { tx, lines }
}

impl Stdio {
    /// The `result` of one request.
    async fn rpc(&mut self, method: &str) -> Value {
        let req = json!({"jsonrpc":"2.0","id":7,"method":method});
        self.tx
            .write_all(format!("{req}\n").as_bytes())
            .await
            .expect("write");
        loop {
            let line = tokio::time::timeout(WITHIN, self.lines.next_line())
                .await
                .expect("line in time")
                .expect("read")
                .expect("line");
            let v: Value = serde_json::from_str(&line).expect("json");
            if v["id"] == 7 {
                return v["result"].clone();
            }
        }
    }
}

fn names(result: &Value, list: &str, key: &str) -> Vec<String> {
    result[list]
        .as_array()
        .unwrap_or_else(|| panic!("no {list}: {result}"))
        .iter()
        .map(|t| t[key].as_str().expect("name").to_owned())
        .collect()
}

#[tokio::test]
async fn stdio_lists_follow_the_policy_and_allow_all_lists_everything() {
    // AllowAll: visible == the whole registry, in registry order.
    let mut s = stdio(Arc::new(AllowAll)).await;
    let all_tools = names(&s.rpc("tools/list").await, "tools", "name");
    assert_eq!(all_tools.len(), 3, "{all_tools:?}");
    let all_resources = names(&s.rpc("resources/list").await, "resources", "uri");
    assert_eq!(all_resources, [RES_A, RES_B]);

    // Enforcing policy hiding some: exactly the visible subset, order preserved.
    let mut s = stdio(Arc::new(Carol)).await;
    let tools = names(&s.rpc("tools/list").await, "tools", "name");
    let expected: Vec<String> = all_tools
        .iter()
        .filter(|n| n.as_str() == READ_TOOL || n.as_str() == WRITE_TOOL)
        .cloned()
        .collect();
    assert_eq!(tools, expected);
    assert!(!tools.iter().any(|n| n == REMOTE_TOOL));
    assert_eq!(
        names(&s.rpc("resources/list").await, "resources", "uri"),
        [RES_B]
    );
}

/// Enforcing engine for the stdio test: hides `SN-Get-Record` and
/// `servicenow://instance`.
struct Carol;
impl PolicyEngine for Carol {
    fn decide<'a>(
        &'a self,
        c: &'a RequestContext,
        o: &'a OperationSet,
    ) -> BoxFut<'a, Result<PolicyDecision, PolicyError>> {
        PerUser.decide(c, o)
    }
    fn visible<'a>(&'a self, _: &'a RequestContext, subject: &'a Subject) -> BoxFut<'a, bool> {
        let hidden = matches!(subject, Subject::Tool(REMOTE_TOOL))
            || matches!(subject, Subject::Resource(u) if u == RES_A);
        Box::pin(async move { !hidden })
    }
    fn enforces(&self) -> bool {
        true
    }
}

// ------------------------------------------------------- multi-user HTTP

/// `Bearer <sub>`: a federated principal named `<sub>`.
struct Bearer;
impl HttpAuthenticator for Bearer {
    fn authenticate<'a>(
        &'a self,
        req: AuthRequest<'a>,
    ) -> BoxFut<'a, Result<VerifiedHttpIdentity, AuthChallenge>> {
        let sub = req
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string);
        Box::pin(async move {
            let Some(sub) = sub else {
                return Err(AuthChallenge::unauthorized(None));
            };
            Ok(VerifiedHttpIdentity::new(
                Principal::Federated {
                    org: "acme".into(),
                    iss: "https://idp.example".into(),
                    sub,
                },
                ClientIdentity {
                    client_id: "c".into(),
                    verified: true,
                },
                Vec::new().into(),
            ))
        })
    }
}

fn app() -> Router {
    let d = dispatcher(Arc::new(PerUser));
    let streamable = McpHandler::builder(
        TrustedRoot::streamable_http(DeploymentMode::MultiUser),
        d.clone(),
    )
    .build();
    let sse = McpHandler::builder(TrustedRoot::legacy_sse(DeploymentMode::MultiUser), d).build();
    let mut config = HttpConfig::new(
        HttpSecurity::new(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            &[],
            &[],
        )
        .expect("security"),
        Arc::new(|| json!({})),
        Arc::new(|| Box::pin(async { Ok(json!([{"name": "global"}])) })),
    );
    config.keepalive = Duration::from_secs(3600);
    HttpApp::with_authenticator(streamable, sse, config, Arc::new(Bearer)).router()
}

async fn send(app: &Router, auth: &str, headers: &[(&str, &str)], body: &Value) -> Response<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, LOCAL)
        .header(header::AUTHORIZATION, format!("Bearer {auth}"))
        .header(header::CONTENT_TYPE, "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut req = req.body(Body::from(body.to_string())).expect("request");
    req.extensions_mut().insert(ConnectInfo(PeerAddrs {
        local: LOCAL.parse().expect("addr"),
        remote: "127.0.0.1:50000".parse().expect("addr"),
    }));
    app.clone().call(req).await.expect("infallible")
}

async fn read_until(body: &mut Body, needle: &str) -> String {
    let mut seen = String::new();
    tokio::time::timeout(WITHIN, async {
        while !seen.contains(needle) {
            let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx))
                .await
                .unwrap_or_else(|| panic!("stream ended before {needle:?}: {seen}"))
                .expect("frame");
            if let Ok(data) = frame.into_data() {
                seen.push_str(&String::from_utf8_lossy(&data));
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}: {seen}"));
    seen
}

/// An initialized Streamable `2025-11-25` session for `auth`; returns its id.
async fn session(app: &Router, auth: &str) -> String {
    let r = send(app, auth, &[("Accept", BOTH)], &initialize()).await;
    assert_eq!(r.status(), StatusCode::OK);
    let sid = r
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("session id")
        .to_owned();
    let mut body = r.into_body();
    read_until(&mut body, "serverInfo").await;
    let r = send(
        app,
        auth,
        &session_headers(&sid),
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    sid
}

fn session_headers(sid: &str) -> [(&str, &str); 3] {
    [
        ("Accept", BOTH),
        ("mcp-session-id", sid),
        ("mcp-protocol-version", SESSION),
    ]
}

/// The `result` of a `method` request on the session.
async fn http_rpc(app: &Router, auth: &str, sid: &str, method: &str) -> Value {
    let r = send(
        app,
        auth,
        &session_headers(sid),
        &json!({"jsonrpc":"2.0","id":7,"method":method}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body();
    let text = read_until(&mut body, "\"result\"").await;
    let data = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .find(|d| d.contains("\"result\""))
        .unwrap_or_else(|| panic!("no data line: {text}"));
    let v: Value = serde_json::from_str(data.trim()).unwrap_or_else(|e| panic!("{e}: {text}"));
    v["result"].clone()
}

#[tokio::test]
async fn multi_user_lists_are_filtered_per_principal() {
    let app = app();
    for (who, tools, resources) in [
        (
            "alice",
            vec![READ_TOOL, REMOTE_TOOL, WRITE_TOOL],
            vec![RES_A, RES_B],
        ),
        ("carol", vec![READ_TOOL], vec![RES_A]),
        // Allowed nothing: sees no tool and no resource.
        ("bob", vec![], vec![]),
    ] {
        let sid = session(&app, who).await;
        let mut got = names(
            &http_rpc(&app, who, &sid, "tools/list").await,
            "tools",
            "name",
        );
        got.sort();
        let mut want: Vec<String> = tools.iter().map(|s| (*s).to_owned()).collect();
        want.sort();
        assert_eq!(got, want, "{who} tools");
        let mut got = names(
            &http_rpc(&app, who, &sid, "resources/list").await,
            "resources",
            "uri",
        );
        got.sort();
        let mut want: Vec<String> = resources.iter().map(|s| (*s).to_owned()).collect();
        want.sort();
        assert_eq!(got, want, "{who} resources");
    }
}
