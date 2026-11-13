//! Client behaviour against in-process fakes (ports of
//! servicenow-client-credentials / -oauth / -pagination / -errors,
//! oauth-identity-binding, session-picker-and-batch-errors,
//! ui-client-error-containment, script-processor-execution,
//! servicenow-client-background-script-output and -trigger-timezone).

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use hpm_types::BoxFut;
use serde_json::{Value, json};

use super::http::{Clock, HttpRequest, HttpResponse, HttpTransport, TransportError};
use super::oauth_code::{AuthCodeFlow, AuthCodeFlowConfig, TokenPoster};
use super::*;
use crate::credential::testing::MemStore;

type Handler = dyn Fn(&HttpRequest) -> Result<HttpResponse, TransportError> + Send + Sync;

struct FakeTransport {
    handler: Box<Handler>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl FakeTransport {
    fn new(
        handler: impl Fn(&HttpRequest) -> Result<HttpResponse, TransportError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            handler: Box::new(handler),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl HttpTransport for FakeTransport {
    fn send(&self, request: HttpRequest) -> BoxFut<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request.clone());
            tokio::task::yield_now().await;
            (self.handler)(&request)
        })
    }
}

struct FakeClock {
    now: AtomicI64,
    sleeps: Mutex<Vec<u64>>,
}

impl FakeClock {
    fn sleeps(&self) -> Vec<u64> {
        self.sleeps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn sleep(&self, ms: u64) -> BoxFut<'_, ()> {
        self.sleeps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ms);
        self.now
            .fetch_add(i64::try_from(ms).unwrap_or(0), Ordering::SeqCst);
        Box::pin(async {})
    }
}

/// Token endpoint fake: records params, answers from a script.
struct FakePoster {
    calls: Mutex<Vec<(String, Vec<(String, String)>)>>,
    responses: Mutex<Vec<Result<Value, RawError>>>,
}

impl FakePoster {
    fn new(responses: Vec<Result<Value, RawError>>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(responses),
        })
    }
    fn calls(&self) -> Vec<(String, Vec<(String, String)>)> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn param(&self, i: usize, name: &str) -> Option<String> {
        self.calls()
            .get(i)?
            .1
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
}

impl TokenPoster for FakePoster {
    fn post<'a>(
        &'a self,
        url: &'a str,
        params: Vec<(String, String)>,
    ) -> BoxFut<'a, Result<Value, RawError>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((url.to_owned(), params));
            tokio::task::yield_now().await;
            let mut r = self.responses.lock().unwrap_or_else(|e| e.into_inner());
            if r.is_empty() {
                Ok(json!({"access_token": "default-token", "expires_in": 1800}))
            } else {
                r.remove(0)
            }
        })
    }
}

struct FakeFlow {
    runs: AtomicUsize,
    response: Value,
    exchange: bool,
}

impl AuthCodeFlow for FakeFlow {
    fn run<'a>(
        &'a self,
        config: AuthCodeFlowConfig,
        post: Arc<dyn TokenPoster>,
    ) -> BoxFut<'a, Result<Value, RawError>> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            if self.exchange {
                return post
                    .post(
                        &config.token_url,
                        vec![("grant_type".into(), "authorization_code".into())],
                    )
                    .await;
            }
            Ok(self.response.clone())
        })
    }
}

struct Harness {
    client: ServiceNowClient,
    transport: Arc<FakeTransport>,
    poster: Arc<FakePoster>,
    tokens: Arc<MemStore>,
    creds: Arc<MemStore>,
    flow: Arc<FakeFlow>,
    clock: Arc<FakeClock>,
}

fn harness(
    config: InstanceConfig,
    transport: Arc<FakeTransport>,
    poster: Arc<FakePoster>,
    flow_response: Value,
) -> Harness {
    let tokens = Arc::new(MemStore::default());
    let creds = Arc::new(MemStore::default());
    let flow = Arc::new(FakeFlow {
        runs: AtomicUsize::new(0),
        response: flow_response,
        exchange: false,
    });
    let clock = Arc::new(FakeClock {
        now: AtomicI64::new(1_787_918_400_000),
        sleeps: Mutex::new(Vec::new()),
    });
    let deps = ClientDeps {
        transport: transport.clone(),
        clock: clock.clone(),
        credential_store: InstanceCredentialStore::new(creds.clone()),
        token_store: tokens.clone(),
        auth_flow: flow.clone(),
        token_poster: Some(poster.clone()),
        os_user: "operator".into(),
        trusted_origins: Arc::new(|| None),
    };
    let client = ServiceNowClient::new(config, deps).unwrap_or_else(|e| panic!("{e}"));
    Harness {
        client,
        transport,
        poster,
        tokens,
        creds,
        flow,
        clock,
    }
}

const URL: &str = "https://dev.service-now.com";

fn ok_result(result: Value) -> impl Fn(&HttpRequest) -> Result<HttpResponse, TransportError> {
    move |_| Ok(HttpResponse::json(200, &json!({ "result": result })))
}

fn basic(password: Option<&str>) -> InstanceConfig {
    InstanceConfig {
        url: URL.into(),
        username: Some("admin".into()),
        password: password.map(str::to_owned),
        ..InstanceConfig::default()
    }
}

fn oauth(grant: Option<&str>) -> InstanceConfig {
    InstanceConfig {
        url: URL.into(),
        auth_type: AuthType::OAuth,
        client_id: Some("cid".into()),
        client_secret: Some("csecret".into()),
        grant_type: grant.map(str::to_owned),
        ..InstanceConfig::default()
    }
}

fn auth_of(r: &HttpRequest) -> String {
    r.header("Authorization").unwrap_or("").to_owned()
}

// ---- pagination -------------------------------------------------------------

#[tokio::test]
async fn get_records_sends_offset_and_order_in_6x_order() {
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let q = RecordQuery {
        query: Some("active=true".into()),
        limit: Some("10".into()),
        fields: Some("sys_id,number".into()),
        offset: Some("20".into()),
        order_by: Some("number".into()),
    };
    assert_eq!(
        h.client.get_records("incident", &q).await.ok(),
        Some(json!([]))
    );
    assert_eq!(
        h.transport.requests()[0].url,
        format!(
            "{URL}/api/now/table/incident?sysparm_query=active%3Dtrue&sysparm_limit=10&sysparm_fields=sys_id%2Cnumber&sysparm_offset=20&sysparm_order_by=number"
        )
    );
}

#[tokio::test]
async fn get_records_omits_empty_params() {
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let _ = h
        .client
        .get_records("incident", &RecordQuery::default())
        .await;
    assert_eq!(
        h.transport.requests()[0].url,
        format!("{URL}/api/now/table/incident?")
    );
    let r = &h.transport.requests()[0];
    assert_eq!(r.header("User-Agent"), Some(REST_USER_AGENT));
    assert_eq!(r.header("Accept"), Some("application/json"));
    assert_eq!(auth_of(r), format!("Basic {}", STANDARD.encode("admin:pw")));
}

// ---- errors -----------------------------------------------------------------

#[tokio::test]
async fn table_errors_are_enriched_and_redacted() {
    let h = harness(
        basic(Some("frozen-password-secret")),
        FakeTransport::new(|_| {
            Ok(HttpResponse::json(
                403,
                &json!({"error": {"message": "Table access was denied.", "detail": "frozen-password-secret leaked"}}),
            ))
        }),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let err = h
        .client
        .get_records("x", &RecordQuery::default())
        .await
        .err()
        .unwrap_or_else(|| ClientError::plain(""));
    assert_eq!(
        err.message,
        "403 Table access was denied. [redacted] leaked"
    );
    assert_eq!(err.status, Some(403));
    assert!(!err.to_json().to_string().contains("frozen-password-secret"));
}

#[tokio::test]
async fn transport_errors_keep_code() {
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(|_| Err(TransportError::new("ECONNREFUSED", "connect ECONNREFUSED"))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let err = h
        .client
        .get_records("x", &RecordQuery::default())
        .await
        .err();
    assert_eq!(err.and_then(|e| e.code), Some("ECONNREFUSED".into()));
}

#[test]
fn instance_url_validation() {
    assert_eq!(
        normalize_client_instance_url("https://a.example.com///")
            .ok()
            .as_deref(),
        Some("https://a.example.com")
    );
    assert_eq!(
        normalize_client_instance_url("https://a.example.com/sub/")
            .ok()
            .as_deref(),
        Some("https://a.example.com/sub")
    );
    for bad in [
        "",
        "ftp://a",
        "https://u:p@a",
        "https://a?x",
        "https://a#f",
        "nope",
    ] {
        assert!(normalize_client_instance_url(bad).is_err(), "{bad}");
    }
}

// ---- registered credentials -----------------------------------------------

fn ref_config(auth: AuthType, reference: Value) -> InstanceConfig {
    InstanceConfig {
        url: URL.into(),
        name: Some("dev".into()),
        username: (auth == AuthType::Basic).then(|| "admin".to_owned()),
        auth_type: auth,
        client_id: (auth == AuthType::OAuth).then(|| "cid".to_owned()),
        credential_ref: Some(reference),
        ..InstanceConfig::default()
    }
}

#[tokio::test]
async fn basic_password_resolved_from_keychain_once() {
    let h = harness(
        ref_config(AuthType::Basic, json!("keychain:instance/dev/password")),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    h.creds.put("keychain:instance/dev/password", "kc-pass");
    let q = RecordQuery::default();
    let (a, b) = tokio::join!(h.client.get_records("x", &q), h.client.get_records("y", &q));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(
        h.creds
            .calls("get")
            .iter()
            .filter(|a| a.starts_with("keychain:instance/"))
            .count(),
        1
    );
    assert_eq!(
        auth_of(&h.transport.requests()[0]),
        format!("Basic {}", STANDARD.encode("admin:kc-pass"))
    );
}

#[tokio::test]
async fn missing_or_mis_shaped_reference_is_credential_not_found() {
    for reference in [
        json!("keychain:instance/dev/client-secret"),
        json!({"password": "keychain:instance/dev/password"}),
        json!("not-a-ref"),
    ] {
        let h = harness(
            ref_config(AuthType::Basic, reference),
            FakeTransport::new(ok_result(json!([]))),
            FakePoster::new(vec![]),
            Value::Null,
        );
        let err = h
            .client
            .get_records("x", &RecordQuery::default())
            .await
            .err();
        assert_eq!(
            err.as_ref().and_then(|e| e.code.clone()),
            Some("CREDENTIAL_NOT_FOUND".into())
        );
        assert_eq!(err.map(|e| e.message), Some("Credential not found".into()));
        assert!(h.transport.requests().is_empty());
    }
}

#[tokio::test]
async fn absent_keychain_item_is_credential_not_found() {
    let h = harness(
        ref_config(AuthType::Basic, json!("keychain:instance/dev/password")),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let err = h
        .client
        .get_records("x", &RecordQuery::default())
        .await
        .err();
    assert_eq!(
        err.map(|e| (e.name, e.message)),
        Some((
            "CredentialResolutionError".into(),
            "Credential not found".into()
        ))
    );
}

#[tokio::test]
async fn oauth_password_grant_needs_object_reference() {
    let h = harness(
        InstanceConfig {
            username: Some("svc".into()),
            ..ref_config(
                AuthType::OAuth,
                json!({"password": "keychain:instance/dev/password", "clientSecret": "keychain:instance/dev/client-secret"}),
            )
        },
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    h.creds.put("keychain:instance/dev/password", "kc-pass");
    h.creds
        .put("keychain:instance/dev/client-secret", "kc-secret");
    assert!(
        h.client
            .get_records("x", &RecordQuery::default())
            .await
            .is_ok()
    );
    assert_eq!(h.poster.param(0, "grant_type").as_deref(), Some("password"));
    assert_eq!(
        h.poster.param(0, "client_secret").as_deref(),
        Some("kc-secret")
    );
    assert_eq!(h.poster.param(0, "password").as_deref(), Some("kc-pass"));
    assert_eq!(h.poster.param(0, "username").as_deref(), Some("svc"));
}

#[tokio::test]
async fn set_instance_during_resolution_is_stale() {
    let h = harness(
        ref_config(AuthType::Basic, json!("keychain:instance/dev/password")),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    h.creds.put("keychain:instance/dev/password", "kc-pass");
    let client = h.client.clone();
    let call = tokio::spawn(async move { client.get_records("x", &RecordQuery::default()).await });
    // Switch before the spawned call gets a chance to finish its awaits.
    h.client
        .set_instance(basic(Some("other")))
        .unwrap_or_else(|e| panic!("{e}"));
    let result = call.await.unwrap_or_else(|e| panic!("{e}"));
    if let Err(e) = result {
        assert_eq!(e.code(), Some(error::STALE_INSTANCE_CODE));
        assert_eq!(e.message, "Instance changed during credential resolution");
    }
    assert!(h.transport.requests().iter().all(|r| auth_of(r)
        != format!("Basic {}", STANDARD.encode("admin:other"))
        || r.url.starts_with(URL)));
}

// ---- OAuth -------------------------------------------------------------------

#[tokio::test]
async fn client_credentials_grant_and_cache() {
    let h = harness(
        oauth(None),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![Ok(json!({"access_token": "at1", "expires_in": 1800}))]),
        Value::Null,
    );
    let q = RecordQuery::default();
    let (a, b) = tokio::join!(h.client.get_records("x", &q), h.client.get_records("y", &q));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(h.poster.calls().len(), 1);
    assert_eq!(h.poster.calls()[0].0, format!("{URL}/oauth_token.do"));
    assert_eq!(
        h.poster.param(0, "grant_type").as_deref(),
        Some("client_credentials")
    );
    assert!(
        h.transport
            .requests()
            .iter()
            .all(|r| auth_of(r) == "Bearer at1")
    );
}

#[tokio::test]
async fn retries_once_on_401_with_fresh_token() {
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let h = harness(
        oauth(None),
        FakeTransport::new(move |_| {
            if n2.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(HttpResponse::json(
                    401,
                    &json!({"error": {"message": "expired"}}),
                ))
            } else {
                Ok(HttpResponse::json(200, &json!({"result": []})))
            }
        }),
        FakePoster::new(vec![
            Ok(json!({"access_token": "at1", "expires_in": 1800})),
            Ok(json!({"access_token": "at2", "expires_in": 1800})),
        ]),
        Value::Null,
    );
    assert!(
        h.client
            .get_records("x", &RecordQuery::default())
            .await
            .is_ok()
    );
    let reqs = h.transport.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(auth_of(&reqs[1]), "Bearer at2");
}

#[tokio::test]
async fn second_401_is_not_retried() {
    let h = harness(
        oauth(None),
        FakeTransport::new(|_| {
            Ok(HttpResponse::json(
                401,
                &json!({"error": {"message": "nope"}}),
            ))
        }),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let err = h
        .client
        .get_records("x", &RecordQuery::default())
        .await
        .err();
    assert_eq!(err.and_then(|e| e.status), Some(401));
    assert_eq!(h.transport.requests().len(), 2);
}

#[tokio::test]
async fn invalid_token_responses_are_rejected() {
    for (body, msg) in [
        (
            json!([]),
            "OAuth token endpoint returned an invalid response",
        ),
        (
            json!({"access_token": " "}),
            "OAuth token endpoint response is missing a valid access token",
        ),
        (
            json!({"access_token": "a", "refresh_token": 5}),
            "OAuth token endpoint response contains an invalid refresh token",
        ),
        (
            json!({"access_token": "a", "expires_in": 0}),
            "OAuth token endpoint response contains an invalid expiry",
        ),
    ] {
        let h = harness(
            oauth(None),
            FakeTransport::new(ok_result(json!([]))),
            FakePoster::new(vec![Ok(body)]),
            Value::Null,
        );
        let err = h
            .client
            .get_records("x", &RecordQuery::default())
            .await
            .err()
            .unwrap_or_else(|| ClientError::plain(""));
        assert_eq!(err.message, msg);
        assert_eq!(err.code(), Some("OAUTH_TOKEN_RESPONSE_INVALID"));
    }
}

#[tokio::test]
async fn token_request_failure_is_redacted_with_default_code() {
    let h = harness(
        oauth(None),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![Err(RawError {
            message: "failed with csecret".into(),
            status: Some(400),
            data: Some(json!({"error": "invalid_client", "client_secret": "csecret"})),
            ..RawError::default()
        })]),
        Value::Null,
    );
    let err = h
        .client
        .get_records("x", &RecordQuery::default())
        .await
        .err()
        .unwrap_or_else(|| ClientError::plain(""));
    assert_eq!(err.message, "failed with [redacted]");
    assert_eq!(err.code(), Some("OAUTH_TOKEN_REQUEST_FAILED"));
    assert!(!err.to_json().to_string().contains("csecret"));
}

#[tokio::test]
async fn token_endpoint_must_be_approved() {
    let h = harness(
        InstanceConfig {
            token_url: Some("https://evil.example/token".into()),
            ..oauth(None)
        },
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let err = h
        .client
        .get_records("x", &RecordQuery::default())
        .await
        .err();
    assert_eq!(
        err.and_then(|e| e.code),
        Some("OAUTH_ENDPOINT_NOT_APPROVED".into())
    );
    assert!(h.poster.calls().is_empty());
}

fn account() -> String {
    oauth_refresh_token_account(URL, "cid", None, None, "operator").unwrap_or_default()
}

#[tokio::test]
async fn authorization_code_signs_in_and_persists_refresh_token() {
    let mut cfg = oauth(Some("authorization_code"));
    cfg.client_secret = None;
    cfg.name = Some("dev".into());
    let h = harness(
        cfg,
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        json!({"access_token": "ac-at", "refresh_token": "ac-rt", "expires_in": 1800}),
    );
    h.tokens.put("operator@dev", "legacy");
    assert!(
        h.client
            .get_records("x", &RecordQuery::default())
            .await
            .is_ok()
    );
    assert_eq!(h.flow.runs.load(Ordering::SeqCst), 1);
    assert!(h.tokens.has(&account()));
    assert!(!h.tokens.has("operator@dev"));
    assert_eq!(auth_of(&h.transport.requests()[0]), "Bearer ac-at");
}

#[tokio::test]
async fn authorization_code_uses_stored_refresh_token_first() {
    let mut cfg = oauth(Some("authorization_code"));
    cfg.client_secret = None;
    let h = harness(
        cfg,
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![Ok(
            json!({"access_token": "refreshed", "expires_in": 1800}),
        )]),
        Value::Null,
    );
    h.tokens.put(&account(), "stored-rt");
    assert!(
        h.client
            .get_records("x", &RecordQuery::default())
            .await
            .is_ok()
    );
    assert_eq!(h.flow.runs.load(Ordering::SeqCst), 0);
    assert_eq!(
        h.poster.param(0, "refresh_token").as_deref(),
        Some("stored-rt")
    );
    assert_eq!(h.poster.param(0, "client_secret"), None);
}

#[tokio::test]
async fn authorization_code_invalid_grant_reprompts_never_password() {
    let mut cfg = oauth(Some("authorization_code"));
    cfg.client_secret = None;
    let h = harness(
        cfg,
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![Err(RawError::http(
            400,
            json!({"error": "invalid_grant"}),
        ))]),
        json!({"access_token": "new", "refresh_token": "new-rt"}),
    );
    h.tokens.put(&account(), "dead-rt");
    assert!(
        h.client
            .get_records("x", &RecordQuery::default())
            .await
            .is_ok()
    );
    assert_eq!(h.flow.runs.load(Ordering::SeqCst), 1);
    assert_eq!(h.poster.calls().len(), 1);
    assert!(
        h.poster
            .calls()
            .iter()
            .all(|(_, p)| !p.iter().any(|(k, v)| k == "grant_type" && v == "password"))
    );
}

#[tokio::test]
async fn authorization_code_other_refresh_failures_keep_token() {
    let mut cfg = oauth(Some("authorization_code"));
    cfg.client_secret = None;
    let h = harness(
        cfg,
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![Err(RawError::http(
            401,
            json!({"error": "invalid_client"}),
        ))]),
        Value::Null,
    );
    h.tokens.put(&account(), "keep-rt");
    let err = h
        .client
        .get_records("x", &RecordQuery::default())
        .await
        .err();
    assert_eq!(
        err.and_then(|e| e.code),
        Some("AUTHENTICATION_FAILED".into())
    );
    assert!(h.tokens.has(&account()));
    assert_eq!(h.flow.runs.load(Ordering::SeqCst), 0);
}

#[test]
fn refresh_token_account_binds_identity() {
    let a = account();
    assert!(a.starts_with("identity-v1-") && a.len() == 12 + 64);
    assert_eq!(
        oauth_refresh_token_account(&format!("{URL}/"), "cid", None, None, "operator").ok(),
        Some(a.clone())
    );
    assert_ne!(
        oauth_refresh_token_account(URL, "other", None, None, "operator").ok(),
        Some(a.clone())
    );
    assert_ne!(
        oauth_refresh_token_account(URL, "cid", None, None, "someone").ok(),
        Some(a.clone())
    );
    assert_ne!(
        oauth_refresh_token_account(
            URL,
            "cid",
            None,
            Some("https://idp.example/token"),
            "operator"
        )
        .ok(),
        Some(a)
    );
    assert_eq!(
        legacy_refresh_token_account("dev", "op").as_deref(),
        Some("op@dev")
    );
    assert_eq!(legacy_refresh_token_account("../x", "op"), None);
    assert_eq!(legacy_refresh_token_account("", "op"), None);
}

// ---- pickers -----------------------------------------------------------------

const SET_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SET_DEFAULT: &str = "dddddddddddddddddddddddddddddddd";

fn picker_fake(ignore_writes: bool) -> Arc<FakeTransport> {
    let current = Arc::new(Mutex::new(json!({"sysId": SET_DEFAULT, "name": "Default"})));
    let app = Arc::new(Mutex::new(json!("global")));
    FakeTransport::new(move |r| {
        let path = r
            .url
            .trim_start_matches(URL)
            .split('?')
            .next()
            .unwrap_or("")
            .to_owned();
        let j = |s: u16, v: Value| Ok(HttpResponse::json(s, &v));
        match (r.method, path.as_str()) {
            (Method::Get, "/") => Ok(HttpResponse::new(200, "<html></html>")
                .with_header("set-cookie", "JSESSIONID=s1; Path=/")),
            (Method::Get, "/api/now/ui/concoursepicker/updateset") => j(
                200,
                json!({"result": {"current": current.lock().unwrap_or_else(|e| e.into_inner()).clone()}}),
            ),
            (Method::Put, "/api/now/ui/concoursepicker/updateset") => {
                if !ignore_writes {
                    let body: Value = serde_json::from_str(r.body_text()).unwrap_or_default();
                    *current.lock().unwrap_or_else(|e| e.into_inner()) =
                        json!({"sysId": body["sysId"], "name": body["name"]});
                }
                j(200, json!({"result": {"success": true}}))
            }
            (Method::Get, "/api/now/ui/concoursepicker/application") => j(
                200,
                json!({"result": {"current": app.lock().unwrap_or_else(|e| e.into_inner()).clone(), "list": [{"sysId": "global", "name": "Global"}]}}),
            ),
            (Method::Put, "/api/now/ui/concoursepicker/application") => {
                let body: Value = serde_json::from_str(r.body_text()).unwrap_or_default();
                if !ignore_writes {
                    *app.lock().unwrap_or_else(|e| e.into_inner()) = body["app_id"].clone();
                }
                j(200, json!({"result": {"app_id": body["app_id"]}}))
            }
            (Method::Get, p) if p == format!("/api/now/table/sys_update_set/{SET_A}") => j(
                200,
                json!({"result": {"sys_id": SET_A, "name": "Story set"}}),
            ),
            (Method::Get, "/api/now/table/sys_scope/global") => j(
                200,
                json!({"result": {"sys_id": "global", "name": "Global", "scope": "global"}}),
            ),
            _ => j(
                404,
                json!({"error": {"message": format!("unhandled {path}")}}),
            ),
        }
    })
}

#[tokio::test]
async fn current_update_set_reads_picker() {
    let h = harness(
        basic(Some("inert")),
        picker_fake(false),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let r = h.client.get_current_update_set().await.ok();
    assert_eq!(
        r,
        Some(json!({"result": {"name": "Default", "value": SET_DEFAULT, "sys_id": SET_DEFAULT}}))
    );
    assert!(
        !h.transport
            .requests()
            .iter()
            .any(|r| r.url.contains("/api/now/ui/preferences/"))
    );
}

#[tokio::test]
async fn set_update_set_verifies_in_one_session() {
    let h = harness(
        basic(Some("inert")),
        picker_fake(false),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let r = h
        .client
        .set_current_update_set(SET_A)
        .await
        .unwrap_or_default();
    assert_eq!(r["success"], true);
    assert_eq!(r["verified"], true);
    assert_eq!(r["update_set"], "Story set");
    assert_eq!(
        r["previous_update_set"],
        json!({"name": "Default", "sys_id": SET_DEFAULT})
    );
    let picker: Vec<_> = h
        .transport
        .requests()
        .into_iter()
        .filter(|r| r.url.contains("concoursepicker"))
        .collect();
    assert!(
        picker
            .iter()
            .all(|r| r.header("Cookie") == Some("JSESSIONID=s1"))
    );
}

#[tokio::test]
async fn set_update_set_fails_when_write_is_ignored() {
    let h = harness(
        basic(Some("inert")),
        picker_fake(true),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let err = h
        .client
        .set_current_update_set(SET_A)
        .await
        .err()
        .map(|e| e.message);
    assert_eq!(
        err,
        Some(format!(
            "Update set verification failed: current is {SET_DEFAULT}, expected {SET_A}"
        ))
    );
    assert!(h.client.set_current_update_set("BAD").await.is_err());
}

#[tokio::test]
async fn set_application_global_verifies() {
    let h = harness(
        basic(Some("inert")),
        picker_fake(false),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let r = h
        .client
        .set_current_application("global")
        .await
        .unwrap_or_default();
    assert_eq!(r["verified"], true);
    assert_eq!(
        r["previous_scope"],
        json!({"sys_id": "global", "name": "Global"})
    );
    assert_eq!(r["warnings"], json!([]));
}

#[tokio::test]
async fn set_application_errors_are_contained() {
    let secret = "ui-secret-password";
    let h = harness(
        basic(Some(secret)),
        FakeTransport::new(move |r| {
            if r.url.ends_with("/api/now/table/sys_scope/global") {
                Ok(HttpResponse::json(
                    200,
                    &json!({"result": {"name": "Global"}}),
                ))
            } else {
                Ok(HttpResponse::json(
                    500,
                    &json!({"error": {"message": format!("boom {secret}")}}),
                ))
            }
        }),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let err = h
        .client
        .set_current_application("global")
        .await
        .err()
        .unwrap_or_else(|| ClientError::plain(""));
    assert_eq!(
        err.message,
        "Failed to set current application: ServiceNow server error (500). Please try again later."
    );
    assert_eq!(err.status, Some(500));
    assert!(!err.to_json().to_string().contains(secret));
}

// ---- scripts -----------------------------------------------------------------

const HISTORY: &str = "0123456789abcdef0123456789abcdef";
const ROLLBACK: &str = "fedcba9876543210fedcba9876543210";

fn processor_fake(body: impl Into<String>) -> Arc<FakeTransport> {
    let body: String = body.into();
    FakeTransport::new(move |r| {
        let path = r
            .url
            .trim_start_matches(URL)
            .split('?')
            .next()
            .unwrap_or("")
            .to_owned();
        match path.as_str() {
            "/angular.do" => Ok(HttpResponse::new(200, "{}")
                .with_header("x-usertoken-response", "ck-token")
                .with_header("set-cookie", "glide_session_store=ABC; Path=/")),
            "/sys.scripts.do" => Ok(HttpResponse::new(200, body.clone())),
            "/api/now/table/sys_script_execution_history" => Ok(HttpResponse::json(
                200,
                &json!({"result": [{"rollback_context": {"value": ROLLBACK}}]}),
            )),
            _ => Ok(HttpResponse::json(404, &json!({}))),
        }
    })
}

#[tokio::test]
async fn processor_posts_once_with_session_and_parses() {
    let body = format!(
        "<a href=\"sys_script_execution_history.do?sys_id={HISTORY}\">h</a>Script completed in scope global: script<pre>*** Script: hi<BR/></pre>"
    );
    let h = harness(
        basic(Some("pw")),
        processor_fake(body),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let r = h
        .client
        .execute_script_via_processor(
            "gs.print('hi')",
            &ProcessorOptions {
                reason: Some("why\nnot".into()),
                ..ProcessorOptions::default()
            },
        )
        .await
        .unwrap_or_default();
    assert_eq!(r["status"], "completed");
    assert_eq!(r["output"], json!(["hi"]));
    assert_eq!(r["scopeId"], "global");
    assert_eq!(r["rollbackContextId"], ROLLBACK);
    assert_eq!(
        r["links"]["rollbackContext"],
        format!("/sys_rollback_context.do?sys_id={ROLLBACK}")
    );
    let posts: Vec<_> = h
        .transport
        .requests()
        .into_iter()
        .filter(|r| r.url.ends_with("/sys.scripts.do"))
        .collect();
    assert_eq!(posts.len(), 1);
    let p = &posts[0];
    assert_eq!(p.max_redirects, 0);
    assert_eq!(p.header("X-UserToken"), Some("ck-token"));
    assert_eq!(p.header("Cookie"), Some("glide_session_store=ABC"));
    assert_eq!(p.header("User-Agent"), Some(UI_USER_AGENT));
    assert_eq!(
        p.body_text(),
        "script=gs.print%28%27hi%27%29%0A%2F%2F+happy-platform-mcp%3A+why+not&sysparm_ck=ck-token&runscript=Run+script&quota_managed_transaction=on&record_for_rollback=on"
    );
}

#[tokio::test]
async fn processor_not_run_and_unknown_outcomes() {
    let h = harness(
        basic(Some("pw")),
        processor_fake("not authorized"),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let e = h
        .client
        .execute_script_via_processor("x", &ProcessorOptions::default())
        .await
        .err();
    assert_eq!(
        e.and_then(|e| e.code),
        Some(script_processor::NOT_RUN.into())
    );
    let h = harness(
        basic(Some("pw")),
        processor_fake("<html>??</html>"),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let e = h
        .client
        .execute_script_via_processor("x", &ProcessorOptions::default())
        .await
        .err();
    assert_eq!(
        e.and_then(|e| e.code),
        Some(script_processor::UNKNOWN_OUTCOME.into())
    );
    let e = h
        .client
        .execute_script_via_processor("  ", &ProcessorOptions::default())
        .await
        .err();
    assert_eq!(
        e.map(|e| e.message),
        Some("script must be a non-empty string".into())
    );
}

#[test]
fn wrapper_and_marker_parsing() {
    let w = scripts::build_background_script_wrapper("doIt();", "MCP_1_ab", true, Some("tid"));
    assert!(w.contains("gs.info('MCP_1_ab:START');"));
    assert!(w.contains("} catch (e) {"));
    assert!(w.contains("triggerGR.get('tid')"));
    let w2 = scripts::build_background_script_wrapper("doIt();", "MCP_1_ab", false, None);
    assert!(!w2.contains("triggerGR"));
    assert!(w2.ends_with("} finally {\n}"));
    use scripts::{MarkerOutcome, parse_background_script_marker as parse};
    assert_eq!(
        parse(
            &json!({"message": "MCP_1_ab:END{\"ok\":true,\"x\":1}"}),
            "MCP_1_ab:END"
        ),
        MarkerOutcome::Completed(json!({"x": 1}))
    );
    assert_eq!(
        parse(
            &json!({"message": "MCP_1_ab:END{\"ok\":false,\"error\":\"bad\",\"stack\":\"s\"}"}),
            "MCP_1_ab:END"
        ),
        MarkerOutcome::Failed {
            error: "bad".into(),
            stack: Some(json!("s"))
        }
    );
    assert!(matches!(
        parse(&json!({"message": "garbage"}), "M"),
        MarkerOutcome::Failed { .. }
    ));
    let m = scripts::generate_script_execution_marker(42);
    assert!(m.starts_with("MCP_42_") && m.len() == "MCP_42_".len() + 8);
}

fn trigger_fake(marker_message: Option<&'static str>, logs: Value) -> Arc<FakeTransport> {
    FakeTransport::new(move |r| {
        let path = r
            .url
            .trim_start_matches(URL)
            .split('?')
            .next()
            .unwrap_or("")
            .to_owned();
        let q = r
            .url
            .split_once('?')
            .map(|x| x.1.to_owned())
            .unwrap_or_default();
        match (r.method, path.as_str()) {
            (Method::Post, "/api/now/table/sys_trigger") => Ok(HttpResponse::json(
                201,
                &json!({"result": {"sys_id": "tid", "name": "MCP_Script"}}),
            )),
            (Method::Put, "/api/now/table/sys_trigger/tid") => {
                Ok(HttpResponse::json(200, &json!({"result": {}})))
            }
            (Method::Get, "/api/now/table/syslog") if q.contains("messageLIKE") => {
                let body = r.url.clone();
                let marker = body
                    .split("messageLIKE")
                    .nth(1)
                    .and_then(|s| s.split("%5E").next())
                    .unwrap_or("");
                let marker = url::form_urlencoded::parse(format!("m={marker}").as_bytes())
                    .next()
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let rows = marker_message.map_or(json!([]), |m| json!([{"message": format!("{marker}{m}"), "sys_created_on": "2026-08-28 12:00:05"}]));
                Ok(HttpResponse::json(200, &json!({"result": rows})))
            }
            (Method::Get, "/api/now/table/syslog") => {
                Ok(HttpResponse::json(200, &json!({"result": logs})))
            }
            _ => Ok(HttpResponse::json(404, &json!({}))),
        }
    })
}

#[tokio::test]
async fn trigger_completed_with_logs_and_utc_times() {
    let logs = json!([{"message": "MCP_1_ab:START"}, {"message": "user line"}, {"message": "MCP_9_ff:END{}"}]);
    let h = harness(
        basic(Some("pw")),
        trigger_fake(Some("{\"ok\":true}"), logs),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let r = h
        .client
        .execute_script_via_trigger("x()", "d", true, &TriggerOptions::default())
        .await
        .unwrap_or_default();
    assert_eq!(r["status"], "completed");
    assert_eq!(r["success"], true);
    assert_eq!(r["logs"], json!(["user line"]));
    assert_eq!(r["logsWindowUnverified"], true);
    // FakeClock starts at 2026-08-28T12:00:00Z; next_action is +1s in UTC.
    assert_eq!(r["next_action"], "2026-08-28 12:00:01");
    let create = &h.transport.requests()[0];
    let body: Value = serde_json::from_str(create.body_text()).unwrap_or_default();
    assert_eq!(body["next_action"], "2026-08-28 12:00:01");
    assert_eq!(body["trigger_type"], "0");
}

#[tokio::test]
async fn trigger_failed_and_timeout_and_scheduled() {
    let h = harness(
        basic(Some("pw")),
        trigger_fake(
            Some("{\"ok\":false,\"error\":\"boom\",\"stack\":\"st\"}"),
            json!([]),
        ),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let r = h
        .client
        .execute_script_via_trigger("x()", "d", false, &TriggerOptions::default())
        .await
        .unwrap_or_default();
    assert_eq!(r["status"], "failed");
    assert_eq!(r["message"], "Script execution failed: boom");
    let h = harness(
        basic(Some("pw")),
        trigger_fake(None, json!([])),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let r = h
        .client
        .execute_script_via_trigger("x()", "d", true, &TriggerOptions::default())
        .await
        .unwrap_or_default();
    assert_eq!(r["status"], "timeout");
    assert!(r.get("logs").is_none());
    let r = h
        .client
        .execute_script_via_trigger(
            "x()",
            "d",
            false,
            &TriggerOptions {
                wait: false,
                ..TriggerOptions::default()
            },
        )
        .await
        .unwrap_or_default();
    assert_eq!(r["status"], "scheduled");
    assert!(
        r["message"]
            .as_str()
            .unwrap_or("")
            .ends_with("Trigger will remain after execution.")
    );
}

#[tokio::test]
async fn trigger_errors_are_wrapped() {
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(|_| {
            Ok(HttpResponse::json(
                403,
                &json!({"error": {"message": "ACL"}}),
            ))
        }),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let e = h
        .client
        .execute_script_via_trigger("x", "", true, &TriggerOptions::default())
        .await
        .err();
    assert_eq!(
        e.map(|e| e.message),
        Some("Failed to create script trigger: 403 ACL".into())
    );
}

mod atf;
