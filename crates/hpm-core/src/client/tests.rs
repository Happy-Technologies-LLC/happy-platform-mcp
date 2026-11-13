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
use super::oauth_code::{AuthCodeFlow, AuthCodeFlowConfig, FormParams, TokenPoster};
use super::*;
use crate::credential::testing::MemStore;

type Handler = dyn Fn(&HttpRequest) -> Result<HttpResponse, TransportError> + Send + Sync;

struct FakeTransport {
    handler: Box<Handler>,
    requests: Mutex<Vec<HttpRequest>>,
    /// Requests whose URL contains this never complete (stalled server).
    stall: Mutex<Option<&'static str>>,
}

impl FakeTransport {
    fn new(
        handler: impl Fn(&HttpRequest) -> Result<HttpResponse, TransportError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            handler: Box::new(handler),
            requests: Mutex::new(Vec::new()),
            stall: Mutex::new(None),
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
            let stall = *self.stall.lock().unwrap_or_else(|e| e.into_inner());
            if stall.is_some_and(|s| request.url.contains(s)) {
                std::future::pending::<()>().await;
            }
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
    calls: Mutex<Vec<(String, FormParams)>>,
    responses: Mutex<Vec<Result<Value, RawError>>>,
}

impl FakePoster {
    fn new(responses: Vec<Result<Value, RawError>>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(responses),
        })
    }
    fn calls(&self) -> Vec<(String, FormParams)> {
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

/// A secret saved by the CLI for the instance at [`URL`]: bound to its origin.
fn put_bound(creds: &MemStore, reference: &str, value: &str) {
    creds.put(reference, value);
    creds.put(
        &crate::keychain::origin_binding_account(reference),
        &crate::keychain::origin_binding_value(reference, value, URL.trim_end_matches('/')),
    );
}

/// M-B: a binding item an attacker can write without the secret (plain
/// origin, wrong MAC, missing MAC, or one copied from another secret) is
/// unbound: refused, nothing sent.
#[tokio::test]
async fn forged_or_copied_binding_is_refused_and_nothing_is_sent() {
    const REF: &str = "keychain:instance/dev/password";
    let origin = URL.trim_end_matches('/');
    let other = crate::keychain::origin_binding_value(
        "keychain:instance/other/password",
        "other-pass",
        origin,
    );
    let wrong_key = crate::keychain::origin_binding_value(REF, "not-the-secret", origin);
    for planted in [
        origin.to_owned(),
        format!(r#"{{"origin":"{origin}"}}"#),
        format!(r#"{{"origin":"{origin}","mac":"{}"}}"#, "0".repeat(64)),
        wrong_key,
        other,
    ] {
        let h = harness(
            ref_config(AuthType::Basic, json!(REF)),
            FakeTransport::new(ok_result(json!([]))),
            FakePoster::new(vec![]),
            Value::Null,
        );
        h.creds.put(REF, "kc-pass");
        h.creds
            .put(&crate::keychain::origin_binding_account(REF), &planted);
        let err = h
            .client
            .get_records("x", &RecordQuery::default())
            .await
            .expect_err("forged binding");
        assert_eq!(
            err.code(),
            Some(error::CREDENTIAL_ORIGIN_UNBOUND_CODE),
            "{planted}"
        );
        assert!(h.transport.requests().is_empty(), "{planted}");
    }
}

#[tokio::test]
async fn unbound_secret_is_never_sent_and_never_bound_lazily() {
    const REF: &str = "keychain:instance/dev/password";
    let attacker = "https://attacker.example";
    let h = harness(
        InstanceConfig {
            url: attacker.into(),
            ..ref_config(AuthType::Basic, json!(REF))
        },
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    // A 6.x (or orphaned) secret with no binding, its name now registered
    // at an agent-chosen URL.
    h.creds.put(REF, "kc-pass");
    let q = RecordQuery::default();
    for _ in 0..2 {
        let err = h.client.get_records("x", &q).await.expect_err("unbound");
        assert_eq!(err.code(), Some(error::CREDENTIAL_ORIGIN_UNBOUND_CODE));
        assert!(
            err.message.contains("instance credential bind dev"),
            "{}",
            err.message
        );
        assert!(!err.message.contains("kc-pass"));
    }
    assert!(h.transport.requests().is_empty());
    let store = InstanceCredentialStore::new(h.creds.clone());
    assert_eq!(store.bound_origin(REF).await.ok().flatten(), None);
}

#[tokio::test]
async fn saved_secret_is_never_sent_to_a_new_origin() {
    const REF: &str = "keychain:instance/dev/password";
    let h = harness(
        ref_config(AuthType::Basic, json!(REF)),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    // Bound at write time (or by `instance credential bind`): used.
    put_bound(&h.creds, REF, "kc-pass");
    let q = RecordQuery::default();
    assert!(h.client.get_records("x", &q).await.is_ok());

    // Re-registering the name with a new URL keeps the reference: refused
    // before any request reaches the new host.
    let moved = "https://elsewhere.example.com";
    h.client
        .set_instance(InstanceConfig {
            url: moved.into(),
            ..ref_config(AuthType::Basic, json!(REF))
        })
        .unwrap_or_else(|e| panic!("{e}"));
    let err = h.client.get_records("x", &q).await.expect_err("mismatch");
    assert_eq!(err.code(), Some(error::CREDENTIAL_ORIGIN_MISMATCH_CODE));
    assert!(
        err.message.contains("instance credential set dev"),
        "{}",
        err.message
    );
    assert!(!err.message.contains("kc-pass"));
    assert!(
        h.transport
            .requests()
            .iter()
            .all(|r| !r.url.starts_with(moved))
    );

    // Re-saving the credential for the new URL binds it there.
    let store = InstanceCredentialStore::new(h.creds.clone());
    store
        .set_secret(
            REF,
            hpm_types::credential::Secret::new("new-pass".into()),
            moved,
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    h.client
        .set_instance(InstanceConfig {
            url: moved.into(),
            ..ref_config(AuthType::Basic, json!(REF))
        })
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(h.client.get_records("x", &q).await.is_ok());
    assert_eq!(
        store.bound_origin(REF).await.ok().flatten().as_deref(),
        Some(moved)
    );
}

#[tokio::test]
async fn basic_password_resolved_from_keychain_once() {
    let h = harness(
        ref_config(AuthType::Basic, json!("keychain:instance/dev/password")),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    put_bound(&h.creds, "keychain:instance/dev/password", "kc-pass");
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
    put_bound(&h.creds, "keychain:instance/dev/password", "kc-pass");
    put_bound(&h.creds, "keychain:instance/dev/client-secret", "kc-secret");
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
async fn instance_switch_while_in_flight_is_stale_after_send() {
    let slot: Arc<std::sync::OnceLock<ServiceNowClient>> = Arc::default();
    let in_handler = slot.clone();
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(move |_| {
            // The request has left; the operator switches instance meanwhile.
            if let Some(c) = in_handler.get() {
                c.set_instance(basic(Some("other")))
                    .unwrap_or_else(|e| panic!("{e}"));
            }
            Ok(HttpResponse::json(200, &json!({"result": {}})))
        }),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let _ = slot.set(h.client.clone());
    let e = h
        .client
        .create_record("incident", &json!({}))
        .await
        .expect_err("stale");
    assert!(e.is_stale_after_send());
    assert_eq!(e.code(), Some(error::STALE_AFTER_SEND_CODE));
    assert_eq!(e.message, "Instance changed during credential resolution");
    assert_eq!(h.transport.requests().len(), 1);
}

#[tokio::test]
async fn set_instance_during_resolution_is_stale() {
    let h = harness(
        ref_config(AuthType::Basic, json!("keychain:instance/dev/password")),
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    put_bound(&h.creds, "keychain:instance/dev/password", "kc-pass");
    let client = h.client.clone();
    let call = tokio::spawn(async move { client.get_records("x", &RecordQuery::default()).await });
    // Switch before the spawned call gets a chance to finish its awaits.
    h.client
        .set_instance(basic(Some("other")))
        .unwrap_or_else(|e| panic!("{e}"));
    let result = call.await.unwrap_or_else(|e| panic!("{e}"));
    if let Err(e) = result {
        // Before or after the send, depending on scheduling.
        assert!(e.is_stale(), "{e:?}");
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
    // RFC §4: the picker UI session is ended per operation, with its cookie.
    let last = h.transport.requests().pop();
    assert_eq!(
        last.as_ref().map(|r| r.url.clone()),
        Some(format!("{URL}/logout.do"))
    );
    assert!(
        last.and_then(|r| r.header("Cookie").map(str::to_owned))
            .is_some()
    );
}

/// L-3: on the app picker's error path the UI session is still ended in
/// line: exactly one `/logout.do`, with the cookie, as the last request.
#[tokio::test]
async fn set_application_failure_ends_session_last() {
    let transport = FakeTransport::new(|r| {
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
            (Method::Get, "/api/now/table/sys_scope/global") => j(
                200,
                json!({"result": {"sys_id": "global", "name": "Global"}}),
            ),
            (Method::Get, "/api/now/ui/concoursepicker/application") => {
                j(200, json!({"result": {"current": "x", "list": []}}))
            }
            (Method::Put, "/api/now/ui/concoursepicker/application") => {
                j(500, json!({"error": {"message": "boom"}}))
            }
            _ => j(200, json!({})),
        }
    });
    let h = harness(
        basic(Some("inert")),
        transport,
        FakePoster::new(vec![]),
        Value::Null,
    );
    assert!(h.client.set_current_application("global").await.is_err());
    let requests = h.transport.requests();
    assert!(requests.iter().any(|r| r.method == Method::Put));
    let logouts = requests
        .iter()
        .filter(|r| r.url.ends_with("/logout.do"))
        .count();
    let last = requests.last().unwrap();
    assert_eq!(logouts, 1);
    assert_eq!(last.url, format!("{URL}/logout.do"));
    assert!(last.header("Cookie").is_some());
}

/// Review L-9 R6 (L-2): a landing page that answers non-2xx with Set-Cookie
/// opened a UI session; its cookie must reach the logout that ends it.
#[tokio::test]
async fn non_2xx_landing_cookie_reaches_the_logout() {
    let transport = FakeTransport::new(|r| {
        if r.method == Method::Get && r.url.trim_start_matches(URL) == "/" {
            return Ok(HttpResponse::new(503, "<html>busy</html>")
                .with_header("set-cookie", "JSESSIONID=landing1; Path=/; HttpOnly"));
        }
        Ok(HttpResponse::json(200, &json!({})))
    });
    let h = harness(
        basic(Some("inert")),
        transport,
        FakePoster::new(vec![]),
        Value::Null,
    );
    assert!(h.client.set_current_application("global").await.is_err());
    let requests = h.transport.requests();
    let last = requests.last().unwrap();
    assert_eq!(last.url, format!("{URL}/logout.do"));
    assert_eq!(last.header("Cookie"), Some("JSESSIONID=landing1"));
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
    let err = h.client.set_current_update_set(SET_A).await.err();
    assert_eq!(
        err.as_ref().map(|e| e.message.clone()),
        Some(format!(
            "Update set verification failed: current is {SET_DEFAULT}, expected {SET_A}"
        ))
    );
    // The PUT was sent: never marked as "change not sent" (review TF-10).
    assert_eq!(err.and_then(|e| e.code), None);
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
    assert!(
        h.transport
            .requests()
            .iter()
            .any(|r| r.url == format!("{URL}/logout.do"))
    );
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
async fn processor_not_run_fallback_runs_before_the_single_logout() {
    let h = harness(
        basic(Some("pw")),
        processor_fake("not authorized"),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let transport = h.transport.clone();
    let run = h
        .client
        .execute_script_via_processor_or("x", &ProcessorOptions::default(), |e| async move {
            // Stands in for the sys_trigger fallback: no logout has been sent yet.
            let logouts = transport
                .requests()
                .iter()
                .filter(|r| r.url.ends_with("/logout.do"))
                .count();
            (e.code, logouts)
        })
        .await
        .expect("not-run is not an error here");
    let ProcessorRun::NotRun((code, logouts_during)) = run else {
        panic!("expected NotRun");
    };
    assert_eq!(code, Some(script_processor::NOT_RUN.into()));
    assert_eq!(logouts_during, 0);
    let urls: Vec<_> = h.transport.requests().into_iter().map(|r| r.url).collect();
    assert_eq!(urls.iter().filter(|u| u.ends_with("/logout.do")).count(), 1);
    assert!(urls.last().is_some_and(|u| u.ends_with("/logout.do")));
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

const TID: &str = "0123456789abcdef0123456789abcdef";

/// TF-8: only this run's prefixed lines are captured, in order, prefix
/// stripped; other sessions' and other runs' lines are dropped.
#[test]
fn run_log_lines_keep_only_this_run() {
    let messages = [
        "MCP_1_ab:START",
        "MCP_1_ab:LOG mine 1",
        "someone else's gs.info",
        "MCP_9_ff:LOG other run",
        "MCP_1_abc:LOG longer marker",
        "MCP_1_ab:LOG mine 2",
        "MCP_1_ab:END{\"ok\":true}",
    ]
    .map(str::to_owned)
    .to_vec();
    let (lines, verified) = scripts::run_log_lines(messages, "MCP_1_ab");
    assert_eq!(lines, vec!["mine 1".to_owned(), "mine 2".to_owned()]);
    assert!(verified);
}

#[test]
fn wrapper_and_marker_parsing() {
    let w = scripts::build_background_script_wrapper("doIt();", "MCP_1_ab", Some(TID));
    assert!(w.contains("gs.info('MCP_1_ab:START');"));
    assert!(w.contains("} catch (e) {"));
    assert!(w.contains(&format!("triggerGR.get('{TID}')")));
    // The script's own logging is prefixed with the run marker (TF-8).
    assert!(w.contains("(function (gs) {\n  doIt();\n  }).call(this, "));
    assert!(w.contains("})(gs, 'MCP_1_ab:LOG '));\n"));
    let w2 = scripts::build_background_script_wrapper("doIt();", "MCP_1_ab", None);
    assert!(!w2.contains("triggerGR"));
    assert!(w2.ends_with("} finally {\n}"));
    // CO-9: a malformed server-supplied sys_id never reaches the source.
    let w3 = scripts::build_background_script_wrapper("doIt();", "MCP_1_ab", Some("x');evil('"));
    assert!(!w3.contains("triggerGR") && !w3.contains("evil"));
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
                &json!({"result": {"sys_id": TID, "name": "MCP_Script"}}),
            )),
            (Method::Put, p) if p == format!("/api/now/table/sys_trigger/{TID}") => {
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
    // Interleaved foreign lines (other sessions' gs.info, other runs' markers
    // and logs) never enter the result (TF-8).
    let logs = json!([
        {"message": "MCP_1_ab:START"},
        {"message": "other session line"},
        {"message": "MCP_9_ff:LOG foreign run"},
        {"message": "MCP_9_ff:END{}"}
    ]);
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
    assert_eq!(r["logs"], json!([]));
    assert_eq!(r["logsWindowUnverified"], true);
    assert!(
        h.transport
            .requests()
            .iter()
            .any(|q| q.url.contains("messageSTARTSWITHMCP_")),
        "the log query is bounded to this run's marker"
    );
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

#[tokio::test]
async fn trigger_errors_after_insert_are_tagged_result_unknown() {
    // Insert refused: never inserted, no tag.
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(|_| Ok(HttpResponse::json(403, &json!({})))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let e = h
        .client
        .execute_script_via_trigger("x", "", true, &TriggerOptions::default())
        .await
        .expect_err("insert refused");
    assert_ne!(e.code(), Some(error::TRIGGER_INSERTED_RESULT_UNKNOWN_CODE));
    assert!(
        e.reason
            .as_deref()
            .is_some_and(|r| r.starts_with("name MCP_Script_"))
    );

    // Insert timed out: outcome unknown (no status), the trigger is named.
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(|_| Err(TransportError::new("ETIMEDOUT", "ETIMEDOUT: timed out"))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let e = h
        .client
        .execute_script_via_trigger("x", "", true, &TriggerOptions::default())
        .await
        .expect_err("insert timed out");
    assert_eq!(e.status, None);
    assert!(
        e.reason
            .as_deref()
            .is_some_and(|r| r.starts_with("name MCP_Script_"))
    );

    // Inserted, then the wrap (auto_delete) or the syslog poll fails.
    for (auto_delete, failing) in [(true, Method::Put), (false, Method::Get)] {
        let h = harness(
            basic(Some("pw")),
            FakeTransport::new(move |r| {
                if r.method == Method::Post {
                    Ok(HttpResponse::json(201, &json!({"result": {"sys_id": TID}})))
                } else if r.method == failing {
                    Ok(HttpResponse::json(
                        403,
                        &json!({"error": {"message": "ACL"}}),
                    ))
                } else {
                    Ok(HttpResponse::json(200, &json!({"result": {}})))
                }
            }),
            FakePoster::new(vec![]),
            Value::Null,
        );
        let e = h
            .client
            .execute_script_via_trigger("x", "", auto_delete, &TriggerOptions::default())
            .await
            .expect_err("post-insert failure");
        assert_eq!(e.code(), Some(error::TRIGGER_INSERTED_RESULT_UNKNOWN_CODE));
        assert_eq!(e.status, Some(403));
        assert_eq!(
            e.message,
            "The sys_trigger record was created, but a later step failed: 403 ACL"
        );
        assert_eq!(e.reason.as_deref(), Some(format!("sys_id {TID}").as_str()));
    }
}

// ---- url-bounds (tests/url-bounds.test.js:198-219) ----------------------------

#[tokio::test]
async fn credentials_never_cross_origin_or_escape_the_path_prefix() {
    let mut cfg = basic(Some("pw"));
    cfg.url = format!("{URL}/tenant{}", "/".repeat(3000));
    let h = harness(
        cfg,
        FakeTransport::new(ok_result(json!([]))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    h.client
        .get_records("incident", &RecordQuery::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let sent = h.transport.requests();
    assert_eq!(sent.len(), 1);
    assert!(!auth_of(&sent[0]).is_empty());
    assert_eq!(sent[0].url, format!("{URL}/tenant/api/now/table/incident?"));

    let slash_run = "/".repeat(100_000);
    for path in [
        "https://evil.example.com/api/now/table/incident".to_owned(),
        "//evil.example.com/x".to_owned(),
        format!("{URL}/other/api"),
        format!("{slash_run}evil.example.com/api"),
        "/../other/api".to_owned(),
    ] {
        let start = std::time::Instant::now();
        let err = h.client.rest(Method::Get, &path, None).await.err();
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "{path:.40}"
        );
        assert_eq!(
            err.and_then(|e| e.code).as_deref(),
            Some(super::error::STALE_INSTANCE_CODE),
            "{path:.40}"
        );
        assert_eq!(h.transport.requests().len(), 1, "{path:.40}");
    }
    // Authority-looking relative paths are joined under the base path.
    for (path, expected) in [
        ("@evil.example/x", "/tenant/@evil.example/x"),
        (".evil.example/x", "/tenant/.evil.example/x"),
        (":8443/x", "/tenant/:8443/x"),
        ("/x#frag", "/tenant/x"),
    ] {
        h.client
            .rest(Method::Get, path, None)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let last = h.transport.requests().pop().map(|r| r.url);
        assert_eq!(last, Some(format!("{URL}{expected}")));
    }
}

#[tokio::test]
async fn table_and_sys_id_segments_cannot_retarget() {
    let mut cfg = basic(Some("pw"));
    cfg.url = format!("{URL}/tenant");
    let h = harness(
        cfg,
        FakeTransport::new(ok_result(json!({}))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let params = RecordParams::default();
    for (table, id, expected) in [
        (
            "incident",
            "../../x",
            "/tenant/api/now/table/incident/..%2F..%2Fx",
        ),
        (
            "incident",
            "a#frag",
            "/tenant/api/now/table/incident/a%23frag",
        ),
        (
            "incident",
            "a?sysparm_limit=1",
            "/tenant/api/now/table/incident/a%3Fsysparm_limit%3D1",
        ),
        (
            "@evil.example",
            "a",
            "/tenant/api/now/table/%40evil.example/a",
        ),
        // 6.2.0 encodeURIComponent keeps `!~*'().` unescaped.
        ("bad table!", "a", "/tenant/api/now/table/bad%20table!/a"),
        ("x", "~*'()", "/tenant/api/now/table/x/~*'()"),
        // An already-encoded dot segment is re-encoded, never decoded.
        ("x", "%2e%2e", "/tenant/api/now/table/x/%252e%252e"),
        ("x", "100%", "/tenant/api/now/table/x/100%25"),
    ] {
        h.client
            .get_record(table, id, &params)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let last = h.transport.requests().pop().map(|r| r.url);
        assert_eq!(last, Some(format!("{URL}{expected}")));
    }
    for id in ["..", "."] {
        assert!(h.client.get_record("incident", id, &params).await.is_err());
    }
    assert!(h.client.delete_record("..", "a").await.is_err());
    assert_eq!(h.transport.requests().len(), 8);
}

#[test]
fn debug_output_never_shows_secrets() {
    const SECRET: &str = "planted-secret-7f3a";
    let mut cfg = oauth(None);
    cfg.password = Some(SECRET.into());
    cfg.client_secret = Some(SECRET.into());
    cfg.credential_ref = Some(json!({ "password": SECRET }));
    let cache = OAuthCache {
        token: Some(SECRET.into()),
        refresh_token: Some(SECRET.into()),
        expiry: None,
    };
    let mut req = HttpRequest::new(Method::Post, URL);
    req.set_header("Authorization", format!("Bearer {SECRET}"));
    req.set_header("Cookie", format!("JSESSIONID={SECRET}"));
    req.body = Some(SECRET.as_bytes().to_vec());
    let pkce = oauth_code::PkcePair {
        code_verifier: SECRET.into(),
        code_challenge: "challenge".into(),
        code_challenge_method: "S256",
    };
    let mut flow = AuthCodeFlowConfig::new(URL, URL, "cid");
    flow.client_secret = Some(SECRET.into());
    let mut jar = script_processor::CookieJar::default();
    jar.set("JSESSIONID", SECRET);
    let raw = RawError {
        message: SECRET.into(),
        data: Some(json!({ "token": SECRET })),
        ..RawError::default()
    };
    let mut redactor = Redactor::new();
    redactor.add(SECRET);
    for out in [
        format!("{cfg:?}"),
        format!("{cache:?}"),
        format!("{req:?}"),
        format!("{pkce:?}"),
        format!("{flow:?}"),
        format!("{jar:?}"),
        format!("{raw:?}"),
        format!("{redactor:?}"),
        format!(
            "{:?}",
            HttpResponse::new(200, format!(r#"{{"access_token":"{SECRET}"}}"#))
                .with_header("Set-Cookie", &format!("JSESSIONID={SECRET}; Path=/"))
        ),
    ] {
        assert!(!out.contains(SECRET), "{out}");
    }
}

#[test]
fn redaction_never_forgets_a_value_from_the_live_generation() {
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(|_| Ok(HttpResponse::new(200, "{}"))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let st = h.client.state();
    // Far past the old cap of 32: the first token must still be redacted.
    for i in 0..100 {
        st.note_request_value(&format!("Bearer token-{i:03}-zz"));
    }
    st.note_request_value("Bearer token-000-zz"); // deduplicated
    assert_eq!(
        st.request_values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        100
    );
    let out = st.redactor().redact("leak token-000-zz and token-099-zz");
    assert!(!out.contains("token-000-zz"), "{out}");
    assert!(!out.contains("token-099-zz"), "{out}");
}

#[derive(Default)]
struct Sink(Mutex<Vec<(f64, Option<String>)>>);
impl hpm_types::context::ProgressSink for Sink {
    fn emit(&self, progress: f64, _: Option<f64>, message: Option<String>) -> BoxFut<'_, ()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((progress, message));
        Box::pin(async {})
    }
}

#[tokio::test]
async fn progress_is_per_call_and_monotonic() {
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(ok_result(json!({}))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let (a, b) = (Arc::new(Sink::default()), Arc::new(Sink::default()));
    let call_a = h.client.with_progress(a.clone());
    let call_b = h.client.with_progress(b.clone());
    call_a.notify_progress("a1", Some(3), None).await;
    call_b.notify_progress("b1", None, None).await;
    call_a.notify_progress("a2", Some(2), None).await;
    // The shared client and other clones have no route: dropped.
    h.client.notify_progress("none", None, None).await;
    let got = |s: &Sink| s.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(
        got(&a),
        vec![(3.0, Some("a1".into())), (4.0, Some("a2".into()))]
    );
    assert_eq!(got(&b), vec![(1.0, Some("b1".into()))]);
}

/// CO-1: a handler that calls `client(ctx)` twice gets two clones for the
/// same call sink; they share one counter, so progress never goes backwards.
#[tokio::test]
async fn progress_is_shared_by_clones_of_one_call() {
    let h = harness(
        basic(Some("pw")),
        FakeTransport::new(ok_result(json!({}))),
        FakePoster::new(vec![]),
        Value::Null,
    );
    let sink = Arc::new(Sink::default());
    let first = h.client.with_progress(sink.clone());
    first.notify_progress("a", Some(5), None).await;
    let second = h.client.with_progress(sink.clone());
    second.notify_progress("b", None, None).await;
    first.notify_progress("c", Some(1), None).await;
    drop((first, second));
    let got = sink.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(
        got,
        vec![
            (5.0, Some("a".into())),
            (6.0, Some("b".into())),
            (7.0, Some("c".into()))
        ]
    );
}

mod atf;
