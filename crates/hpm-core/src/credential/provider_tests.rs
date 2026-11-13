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

/// Records request URLs; answers every request with an empty result.
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl HttpTransport for Recorder {
    fn send(&self, request: HttpRequest) -> BoxFut<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.url.clone());
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
    let registry = Arc::new(Mutex::new(HashMap::from([
        ("a".to_owned(), entry("a", "https://a.service-now.com")),
        ("b".to_owned(), entry("b", "https://b.service-now.com")),
    ])));
    let lookup = registry.clone();
    let deps = ClientDeps {
        transport: recorder.clone(),
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
    (Arc::new(clients), recorder, registry)
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
