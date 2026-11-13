use std::sync::Mutex;

use hpm_types::context::Principal;
use serde_json::json;

use super::*;
use crate::client::RawError;
use crate::client::http::{
    Clock, HttpRequest, HttpResponse, HttpTransport, Method, TransportError,
};
use crate::client::oauth_code::{AuthCodeFlow, AuthCodeFlowConfig, TokenPoster};
use crate::credential::InstanceCredentialStore;
use crate::credential::testing::MemStore;

/// Records request URLs (`.0`) and each request's (url, Cookie, Authorization)
/// (`.1`); answers every request with an empty result.
#[derive(Default)]
#[allow(clippy::type_complexity)]
struct Recorder(
    Mutex<Vec<String>>,
    Mutex<Vec<(String, Option<String>, Option<String>)>>,
);

impl HttpTransport for Recorder {
    fn send(&self, request: HttpRequest) -> BoxFut<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.url.clone());
            self.1.lock().unwrap_or_else(PoisonError::into_inner).push((
                request.url.clone(),
                request.header("Cookie").map(str::to_owned),
                request.header("Authorization").map(str::to_owned),
            ));
            Ok(HttpResponse::json(200, &json!({"result": []})))
        })
    }
}

struct NoClock;
impl Clock for NoClock {
    fn now_ms(&self) -> i64 {
        1_787_918_400_000
    }
    fn sleep(&self, _ms: u64) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
}

struct NoFlow;
impl AuthCodeFlow for NoFlow {
    fn run<'a>(
        &'a self,
        _config: AuthCodeFlowConfig,
        _post: Arc<dyn TokenPoster>,
    ) -> BoxFut<'a, Result<Value, RawError>> {
        Box::pin(async { Ok(Value::Null) })
    }
}

fn entry(name: &str, url: &str) -> Object {
    json!({"name": name, "url": url, "username": "admin", "password": "pw"})
        .as_object()
        .cloned()
        .unwrap_or_default()
}

type Registry = Arc<Mutex<HashMap<String, Object>>>;

fn setup() -> (Arc<InstanceClients>, Arc<Recorder>, Registry) {
    let recorder = Arc::new(Recorder::default());
    let (clients, registry) = setup_with(recorder.clone());
    (clients, recorder, registry)
}

fn setup_with(transport: Arc<dyn HttpTransport>) -> (Arc<InstanceClients>, Registry) {
    let registry = Arc::new(Mutex::new(HashMap::from([
        ("a".to_owned(), entry("a", "https://a.service-now.com")),
        ("b".to_owned(), entry("b", "https://b.service-now.com")),
    ])));
    let lookup = registry.clone();
    let deps = ClientDeps {
        transport,
        clock: Arc::new(NoClock),
        credential_store: InstanceCredentialStore::new(Arc::new(MemStore::default())),
        token_store: Arc::new(MemStore::default()),
        auth_flow: Arc::new(NoFlow),
        token_poster: None,
        os_user: "operator".into(),
        trusted_origins: Arc::new(|| None),
    };
    let clients = InstanceClients::new(
        deps,
        Arc::new(move |id| {
            lookup
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(id)
                .cloned()
                .ok_or_else(|| ClientError::plain(format!("Instance '{id}' not found")))
        }),
    );
    (Arc::new(clients), registry)
}

fn resolved(id: &str, version: u64) -> ResolvedInstance {
    ResolvedInstance {
        instance_id: id.into(),
        canonical_url: format!("https://{id}.service-now.com"),
        config_version: version,
    }
}

fn binding(id: &str, version: u64, generation: u64) -> CredentialBinding {
    CredentialBinding::new(
        Principal::LocalOperator,
        id.into(),
        version,
        generation,
        None,
        id.into(),
    )
}

#[tokio::test]
async fn pinned_client_reaches_only_its_bound_instance() {
    let (clients, recorder, _) = setup();
    let a = clients
        .client_for(&resolved("a", 1))
        .unwrap_or_else(|e| panic!("{e}"));
    let b = clients
        .client_for(&resolved("b", 1))
        .unwrap_or_else(|e| panic!("{e}"));
    let pa = clients
        .pinned(&binding("a", 1, a.generation()))
        .unwrap_or_else(|e| panic!("{e}"));
    let pb = clients
        .pinned(&binding("b", 1, b.generation()))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(pa.rest(Method::Get, "/api/now/table/x", None).await.is_ok());
    assert!(pb.rest(Method::Get, "/api/now/table/x", None).await.is_ok());
    let urls = recorder
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert!(
        urls[0].starts_with("https://a.service-now.com/"),
        "{urls:?}"
    );
    assert!(
        urls[1].starts_with("https://b.service-now.com/"),
        "{urls:?}"
    );
}

#[tokio::test]
async fn reconfigured_instance_rejects_old_pinned_handle() {
    let (clients, recorder, registry) = setup();
    let a = clients
        .client_for(&resolved("a", 1))
        .unwrap_or_else(|e| panic!("{e}"));
    let old = binding("a", 1, a.generation());
    let pinned = clients.pinned(&old).unwrap_or_else(|e| panic!("{e}"));
    // Instance "a" now points elsewhere (registry edit, new config_version).
    registry
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert("a".into(), entry("a", "https://evil.service-now.com"));
    clients
        .client_for(&resolved("a", 2))
        .unwrap_or_else(|e| panic!("{e}"));
    let err = pinned
        .rest(Method::Get, "/api/now/table/x", None)
        .await
        .err()
        .unwrap_or_else(|| panic!("moved generation must fail"));
    assert!(err.is_stale());
    assert!(matches!(
        clients.pinned(&old),
        Err(CredentialError::GenerationMoved)
    ));
    assert!(
        recorder
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    );
}

#[test]
fn unbound_instance_never_falls_back_to_another_client() {
    let (clients, _, _) = setup();
    clients
        .client_for(&resolved("a", 1))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(
        clients.pinned(&binding("b", 1, 0)),
        Err(CredentialError::GenerationMoved)
    ));
}

#[test]
fn registry_entry_maps_to_instance_config() {
    let mut o = entry("dev", "https://dev.service-now.com");
    o.insert("authType".into(), json!("oauth"));
    o.insert("redirectPort".into(), json!(8455));
    let c = instance_config_from_registry(&o);
    assert_eq!(c.url, "https://dev.service-now.com");
    assert_eq!(c.name.as_deref(), Some("dev"));
    assert_eq!(c.auth_type, AuthType::OAuth);
    assert_eq!(c.redirect_port, Some(8455));
}

#[test]
fn instance_urls_compare_canonically() {
    for (a, b) in [
        (
            "https://dev.service-now.com",
            "https://DEV.Service-Now.com/",
        ),
        (
            "https://dev.service-now.com",
            "https://dev.service-now.com:443",
        ),
        ("http://127.0.0.1:80/tenant/", "http://127.0.0.1/tenant"),
        (
            "https://dev.service-now.com/tenant",
            "https://dev.service-now.com/tenant//",
        ),
    ] {
        assert!(same_instance_url(a, b), "{a} vs {b}");
    }
    for (a, b) in [
        ("https://dev.service-now.com", "http://dev.service-now.com"),
        (
            "https://dev.service-now.com",
            "https://dev.service-now.com:8443",
        ),
        (
            "https://dev.service-now.com",
            "https://evil.service-now.com",
        ),
        (
            "https://dev.service-now.com/a",
            "https://dev.service-now.com/b",
        ),
        ("not a url", "not a url"),
        ("foo://x/y", "foo://x/y"),
    ] {
        assert!(!same_instance_url(a, b), "{a} vs {b}");
    }
}

/// Re-points instance "a" to another host (registry edit, new config_version).
fn repoint_a(clients: &InstanceClients, registry: &Registry) {
    registry
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert("a".into(), entry("a", "https://evil.service-now.com"));
    clients
        .client_for(&resolved("a", 2))
        .unwrap_or_else(|e| panic!("{e}"));
}

fn hosts(recorder: &Recorder) -> Vec<String> {
    recorder
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[tokio::test]
async fn repointed_instance_mid_flow_rejects_every_later_step() {
    let (clients, recorder, registry) = setup();
    let a = clients
        .client_for(&resolved("a", 1))
        .unwrap_or_else(|e| panic!("{e}"));
    let pinned = clients
        .pinned(&binding("a", 1, a.generation()))
        .unwrap_or_else(|e| panic!("{e}"));
    let flow = pinned.client().clone();
    // Step 1 runs on "a".
    assert!(
        flow.rest(Method::Get, "/api/now/table/x", None)
            .await
            .is_ok()
    );
    repoint_a(&clients, &registry);
    // Later steps of the same flow (raw-client methods) fail stale.
    let step = flow.rest(Method::Get, "/api/now/table/x", None).await;
    assert!(step.err().is_some_and(|e| e.is_stale()));
    let ui = flow.open_ui_session().await;
    assert!(ui.err().is_some_and(|e| e.is_stale()));
    let rec = flow
        .get_records("incident", &crate::client::RecordQuery::default())
        .await;
    assert!(rec.err().is_some_and(|e| e.is_stale()));
    // A pinned handle can never re-point itself.
    assert!(
        flow.set_instance(instance_config_from_registry(&entry(
            "a",
            "https://other.service-now.com"
        )))
        .is_err()
    );
    let urls = hosts(&recorder);
    assert!(
        urls.iter().all(|u| !u.starts_with("https://evil.")),
        "{urls:?}"
    );
}

#[tokio::test]
async fn old_cookie_is_logged_out_on_its_own_host_never_the_new_one() {
    let (clients, recorder, registry) = setup();
    let a = clients
        .client_for(&resolved("a", 1))
        .unwrap_or_else(|e| panic!("{e}"));
    let old = binding("a", 1, a.generation());
    let pinned = clients.pinned(&old).unwrap_or_else(|e| panic!("{e}"));
    let provider = SingleUserCredentialProvider::new(clients.clone());
    provider.sessions().insert(7, pinned.client().clone());
    repoint_a(&clients, &registry);
    let session = UiSession {
        cookie_header: Secret::new("JSESSIONID=old".to_owned()),
        csrf_token: Secret::new(String::new()),
        instance_id: "a".into(),
        generation: old.generation(),
        scope: UiSessionScope::PerOperation,
        session_id: 7,
    };
    provider
        .end_ui_session(session)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let sent = recorder
        .1
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    let (url, cookie, auth) = &sent[0];
    assert_eq!(url, "https://a.service-now.com/logout.do");
    assert_eq!(cookie.as_deref(), Some("JSESSIONID=old"));
    // The moved generation's credentials are never attached.
    assert_eq!(auth, &None);
    assert!(provider.sessions().is_empty());
}

#[test]
fn credential_failures_keep_the_6_2_0_text() {
    let not_found = backend(ClientError::credential("CREDENTIAL_NOT_FOUND"));
    assert_eq!(not_found.to_string(), "Credential not found");
    let oauth = backend(ClientError::new(
        "AuthenticationError",
        "OAuth token request failed",
        None,
    ));
    assert_eq!(oauth.to_string(), "OAuth token request failed");
    let stale = backend(ClientError::stale());
    assert!(matches!(stale, CredentialError::GenerationMoved));
    assert_eq!(
        stale.to_string(),
        "Instance changed during credential resolution"
    );
}

#[tokio::test]
async fn basic_without_password_or_ref_is_credential_not_found() {
    let (clients, _, registry) = setup();
    registry
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            "c".into(),
            json!({"name": "c", "url": "https://c.service-now.com", "username": "u"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
    let c = clients
        .client_for(&resolved("c", 1))
        .unwrap_or_else(|e| panic!("{e}"));
    let err = c.auth_header().await.err().map(backend);
    assert_eq!(
        err.map(|e| e.to_string()).as_deref(),
        Some("Credential not found")
    );
}

// ---------------------------------------------------------------------------
// Flow-level pinned-generation tests (lanes C and D): instance "a" is
// re-pointed while one request of a multi-step flow is in flight. Every later
// step must fail stale; nothing may reach the new origin.
// ---------------------------------------------------------------------------

type Answer = Box<dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync>;
type Trip = Box<dyn Fn(&HttpRequest) -> bool + Send + Sync>;

/// Answers via `answer`; the first request matching `trip` re-points "a"
/// (after it was sent, before its response is seen).
struct Scripted {
    log: Mutex<Vec<(Method, String)>>,
    answer: Answer,
    trip: Trip,
    hook: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
    tripped: std::sync::atomic::AtomicBool,
}

impl HttpTransport for Scripted {
    fn send(&self, request: HttpRequest) -> BoxFut<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((request.method, request.url.clone()));
            if (self.trip)(&request)
                && !self.tripped.swap(true, std::sync::atomic::Ordering::SeqCst)
                && let Some(hook) = self.hook.get()
            {
                hook();
            }
            Ok((self.answer)(&request))
        })
    }
}

impl Scripted {
    fn log(&self) -> Vec<(Method, String)> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Requests sent after the tripping one.
    fn after_trip(&self) -> Vec<(Method, String)> {
        let log = self.log();
        let at = log
            .iter()
            .position(|(m, u)| {
                let mut req = HttpRequest::new(*m, u.clone());
                req.max_redirects = 0;
                (self.trip)(&req)
            })
            .unwrap_or_else(|| panic!("trip request never sent: {log:?}"));
        log[at + 1..].to_vec()
    }
}

/// A flow client pinned to "a" (as `ServiceNowAccess::client` hands tools)
/// over a transport that re-points "a" on the first `trip` request.
fn flow_setup(answer: Answer, trip: Trip) -> (ServiceNowClient, Arc<Scripted>) {
    let transport = Arc::new(Scripted {
        log: Mutex::new(Vec::new()),
        answer,
        trip,
        hook: std::sync::OnceLock::new(),
        tripped: std::sync::atomic::AtomicBool::new(false),
    });
    let (clients, registry) = setup_with(transport.clone());
    // The hook owns `clients` (a test-only reference cycle through the
    // transport), so the registry stays live for the whole flow.
    let owner = clients.clone();
    let hook: Box<dyn Fn() + Send + Sync> = Box::new(move || repoint_a(&owner, &registry));
    assert!(transport.hook.set(hook).is_ok());
    let a = clients
        .client_for(&resolved("a", 1))
        .unwrap_or_else(|e| panic!("{e}"));
    let pinned = clients
        .pinned(&binding("a", 1, a.generation()))
        .unwrap_or_else(|e| panic!("{e}"));
    (pinned.client().clone(), transport)
}

fn assert_never_new_origin(log: &[(Method, String)]) {
    assert!(
        log.iter()
            .all(|(_, u)| u.starts_with("https://a.service-now.com/")),
        "{log:?}"
    );
}

const PROGRESS_ID: &str = "2123456789abcdef0123456789abcdef";
const TEST_ID: &str = "0123456789abcdef0123456789abcdef";
const UPDATE_SET_ID: &str = "4123456789abcdef0123456789abcdef";
const PICKER: &str = "/api/now/ui/concoursepicker/updateset";

fn atf_answer() -> Answer {
    Box::new(|r| {
        if r.method == Method::Post && r.url.contains("/api/sn_cicd/tests/run_test") {
            HttpResponse::json(
                200,
                &json!({"result": {"status": "0", "links": {"progress": {"id": PROGRESS_ID, "url": "x"}}}}),
            )
        } else {
            HttpResponse::json(
                200,
                &json!({"result": {"status": "1", "state": "Running", "percent_complete": 10}}),
            )
        }
    })
}

fn atf_payload(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap_or("null"))
        .unwrap_or_default()
}

#[tokio::test]
async fn atf_repointed_between_dispatch_and_poll_is_unknown_and_never_polls_new_host() {
    let (flow, transport) = flow_setup(
        atf_answer(),
        Box::new(|r| r.method == Method::Get && r.url.contains("/api/sn_cicd/progress/")),
    );
    let result = crate::client::cicd_atf::handle_atf_tool(
        "SN-Run-ATF-Test",
        &json!({"test_sys_id": TEST_ID, "wait": true, "timeout_seconds": 60}),
        &flow,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let payload = atf_payload(&result);
    assert_eq!(payload["status"], json!("unknown"), "{payload}");
    assert_eq!(payload["progress_id"], json!(PROGRESS_ID));
    assert!(transport.after_trip().is_empty(), "{:?}", transport.log());
    assert_never_new_origin(&transport.log());
}

#[tokio::test]
async fn atf_repointed_during_dispatch_is_unknown_and_never_polls() {
    let (flow, transport) = flow_setup(
        atf_answer(),
        Box::new(|r| r.method == Method::Post && r.url.contains("/api/sn_cicd/tests/run_test")),
    );
    let result = crate::client::cicd_atf::handle_atf_tool(
        "SN-Run-ATF-Test",
        &json!({"test_sys_id": TEST_ID, "wait": true, "timeout_seconds": 60}),
        &flow,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    // The POST left: the run may have started, so the outcome is unknown.
    let payload = atf_payload(&result);
    assert_eq!(payload["status"], json!("unknown"), "{payload}");
    assert_eq!(result["isError"], json!(true), "{result}");
    assert!(transport.after_trip().is_empty(), "{:?}", transport.log());
    assert_never_new_origin(&transport.log());
}

fn picker_answer() -> Answer {
    Box::new(|r| {
        if r.url.contains("/api/now/table/sys_update_set/") {
            HttpResponse::json(
                200,
                &json!({"result": {"sys_id": UPDATE_SET_ID, "name": "US"}}),
            )
        } else if r.url.contains(PICKER) {
            HttpResponse::json(
                200,
                &json!({"result": {"current": {"sysId": UPDATE_SET_ID, "name": "US"}}}),
            )
        } else {
            HttpResponse::new(200, "<html></html>")
                .with_header("set-cookie", "JSESSIONID=s1; Path=/")
        }
    })
}

/// Picker switch: re-pointed on `trip`; the verify GET never runs, and the
/// only later request is the session's logout on the old host.
async fn picker_switch_repointed_on(trip: Trip) {
    let (flow, transport) = flow_setup(picker_answer(), trip);
    let err = flow
        .set_current_update_set(UPDATE_SET_ID)
        .await
        .err()
        .unwrap_or_else(|| panic!("switch across a re-point must fail"));
    assert!(err.is_stale(), "{err}");
    let later = transport.after_trip();
    assert_eq!(later.len(), 1, "{later:?}");
    assert!(later[0].1.ends_with("/logout.do"), "{later:?}");
    assert_never_new_origin(&transport.log());
}

#[tokio::test]
async fn update_set_switch_repointed_at_session_open_stops_before_put() {
    picker_switch_repointed_on(Box::new(|r| {
        r.method == Method::Get && r.url == "https://a.service-now.com/"
    }))
    .await;
}

#[tokio::test]
async fn update_set_switch_repointed_at_put_never_verifies() {
    picker_switch_repointed_on(Box::new(|r| {
        r.method == Method::Put && r.url.ends_with(PICKER)
    }))
    .await;
}

#[tokio::test]
async fn update_set_switch_on_repointed_instance_sends_nothing() {
    let (flow, transport) = flow_setup(picker_answer(), Box::new(|_| false));
    if let Some(hook) = transport.hook.get() {
        hook();
    }
    let err = flow
        .set_current_update_set(UPDATE_SET_ID)
        .await
        .err()
        .unwrap_or_else(|| panic!("switch on a moved generation must fail"));
    assert!(err.is_stale(), "{err}");
    assert!(transport.log().is_empty(), "{:?}", transport.log());
}

fn trigger_answer() -> Answer {
    Box::new(|_| {
        HttpResponse::json(
            200,
            &json!({"result": {"sys_id": "5123456789abcdef0123456789abcdef", "name": "MCP_Script_1"}}),
        )
    })
}

fn trigger_options() -> crate::client::TriggerOptions {
    crate::client::TriggerOptions {
        wait: true,
        timeout_ms: 1000,
        interval_ms: 100,
        ctx: None,
    }
}

#[tokio::test]
async fn trigger_repointed_after_create_never_updates_or_polls() {
    let (flow, transport) = flow_setup(
        trigger_answer(),
        Box::new(|r| r.method == Method::Post && r.url.contains("/api/now/table/sys_trigger")),
    );
    let err = flow
        .execute_script_via_trigger("gs.info('x')", "", true, &trigger_options())
        .await
        .err()
        .unwrap_or_else(|| panic!("trigger across a re-point must fail"));
    // The insert left: the outcome is unknown, never "not sent".
    assert!(err.is_stale_after_send(), "{err}");
    assert!(transport.after_trip().is_empty(), "{:?}", transport.log());
    assert_never_new_origin(&transport.log());
}

#[tokio::test]
async fn trigger_repointed_at_update_is_result_unknown_and_never_polls() {
    let (flow, transport) = flow_setup(
        trigger_answer(),
        Box::new(|r| r.method == Method::Put && r.url.contains("/api/now/table/sys_trigger/")),
    );
    let err = flow
        .execute_script_via_trigger("gs.info('x')", "", true, &trigger_options())
        .await
        .err()
        .unwrap_or_else(|| panic!("trigger across a re-point must fail"));
    // The row exists: "inserted, result unknown" (never "not sent").
    assert_eq!(
        err.code.as_deref(),
        Some(crate::client::error::TRIGGER_INSERTED_RESULT_UNKNOWN_CODE)
    );
    assert!(transport.after_trip().is_empty(), "{:?}", transport.log());
    assert_never_new_origin(&transport.log());
}
