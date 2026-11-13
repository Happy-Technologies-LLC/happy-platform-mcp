//! Port of `tests/instance-cli.test.js` and `tests/cli.test.js`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use hpm_core::keychain::{
    LazySecretStore, MemoryOp, MemorySecretStore, SERVICE_NAME, SecretStoreError,
};
use hpm_core::token_store::{InMemoryTokenStore, RefreshToken};
use hpm_types::credential::Secret;

use super::*;
use crate::cli::{Dispatch, route};

const SECRET_MSG: &str =
    "Secret flags and values are not accepted in command arguments; use a masked prompt.\n";

fn obj(v: Value) -> Object {
    v.as_object().cloned().unwrap()
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

fn basic() -> Object {
    obj(json!({
        "name": "dev", "url": "https://dev.service-now.com", "authType": "basic", "username": "developer",
        "credentialRef": "keychain:instance/dev/password", "default": true
    }))
}

fn code_instance() -> Object {
    obj(json!({
        "name": "code", "url": "https://code.service-now.com", "authType": "oauth", "grantType": "authorization_code",
        "clientId": "public-client", "default": false
    }))
}

fn registry_error() -> CodedError {
    CodedError::new("REGISTRY_WRITE_FAILED", "registry unavailable")
        .with("path", "/tmp/registry.json")
}

/// `safe_registry_error` resolves the path like Node's `path.resolve`, so on
/// Windows `/tmp/registry.json` gains the current drive.
fn registry_path_field() -> String {
    format!(
        "path={}",
        resolve_path(Path::new("/tmp/registry.json")).display()
    )
}

// --- fakes -------------------------------------------------------------------

/// `registryWith(instances)`: in-memory state, call log, one-shot failures.
#[derive(Default)]
struct FakeRegistry {
    state: Mutex<Vec<Object>>,
    log: Mutex<Vec<(String, Value)>>,
    fail_register: Mutex<Option<CodedError>>,
    fail_remove: Mutex<Option<CodedError>>,
    fail_write: Mutex<Option<CodedError>>,
    raw: Mutex<Value>,
    read_path: PathBuf,
    write_path: PathBuf,
    written: Mutex<Option<Value>>,
    /// Shared ordering clock with the secret store.
    clock: Option<Arc<Clock>>,
}

#[derive(Default)]
struct Clock(Mutex<Vec<String>>);
impl Clock {
    fn tick(&self, what: &str) {
        self.0.lock().unwrap().push(what.to_owned());
    }
    fn position(&self, what: &str) -> Option<usize> {
        self.0.lock().unwrap().iter().position(|w| w == what)
    }
}

impl FakeRegistry {
    fn with(instances: Vec<Object>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(instances),
            read_path: "/tmp/hpm-fake-read.json".into(),
            write_path: "/tmp/hpm-fake-write.json".into(),
            ..Default::default()
        })
    }
    fn calls(&self, op: &str) -> Vec<Value> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(o, _)| o == op)
            .map(|(_, v)| v.clone())
            .collect()
    }
    fn record(&self, op: &str, v: Value) {
        if let Some(c) = &self.clock {
            c.tick(op);
        }
        self.log.lock().unwrap().push((op.into(), v));
    }
}

fn not_found(name: &str) -> CodedError {
    CodedError::new("INSTANCE_NOT_FOUND", format!("Instance '{name}' not found"))
}

impl CliRegistry for FakeRegistry {
    fn list(&self) -> Result<Vec<Object>, CodedError> {
        Ok(self.state.lock().unwrap().clone())
    }
    fn get(&self, name: &str) -> Result<Object, CodedError> {
        self.record("get", json!(name));
        let state = self.state.lock().unwrap();
        state
            .iter()
            .find(|i| i.get("name") == Some(&json!(name)))
            .cloned()
            .ok_or_else(|| not_found(name))
    }
    fn validate(&self, instance: &Value) -> Result<(), CodedError> {
        self.record("validate", instance.clone());
        Ok(())
    }
    fn validate_document(&self, document: &Value) -> Result<Value, CodedError> {
        Ok(document.clone())
    }
    fn register(&self, instance: &Value, make_default: bool) -> Result<Object, CodedError> {
        self.record("register", json!([instance, make_default]));
        if let Some(e) = self.fail_register.lock().unwrap().take() {
            return Err(e);
        }
        let o = instance.as_object().cloned().unwrap();
        self.state.lock().unwrap().push(o.clone());
        Ok(o)
    }
    fn update(&self, name: &str, patch: &Object) -> Result<Object, CodedError> {
        self.record("update", json!([name, patch]));
        let mut state = self.state.lock().unwrap();
        let entry = state
            .iter_mut()
            .find(|i| i.get("name") == Some(&json!(name)))
            .ok_or_else(|| not_found(name))?;
        for (k, v) in patch {
            entry.insert(k.clone(), v.clone());
        }
        Ok(entry.clone())
    }
    fn remove(&self, name: &str) -> Result<(), CodedError> {
        self.record("remove", json!(name));
        if let Some(e) = self.fail_remove.lock().unwrap().take() {
            return Err(e);
        }
        let mut state = self.state.lock().unwrap();
        let before = state.len();
        state.retain(|i| i.get("name") != Some(&json!(name)));
        if state.len() == before {
            return Err(not_found(name));
        }
        Ok(())
    }
    fn raw_document(&self) -> Result<Value, CodedError> {
        Ok(self.raw.lock().unwrap().clone())
    }
    fn read_path(&self) -> PathBuf {
        self.read_path.clone()
    }
    fn write_path(&self) -> PathBuf {
        self.write_path.clone()
    }
    fn write_migrated_document(&self, document: &Value) -> Result<(), CodedError> {
        self.record("write", Value::Null);
        if let Some(e) = self.fail_write.lock().unwrap().take() {
            return Err(e);
        }
        *self.written.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

/// Secret store with ordering clock and failure on the N-th delete.
struct TestSecrets {
    inner: MemorySecretStore,
    clock: Arc<Clock>,
    fail_delete_at: Mutex<Option<usize>>,
    deletes: Mutex<usize>,
}

impl TestSecrets {
    fn new(clock: Arc<Clock>) -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySecretStore::new(),
            clock,
            fail_delete_at: Mutex::new(None),
            deletes: Mutex::new(0),
        })
    }
    fn value(&self, account: &str) -> Option<String> {
        self.inner.value(SERVICE, account)
    }
    fn put(&self, account: &str, value: &str) {
        self.inner.insert(SERVICE, account, value);
    }
    fn count(&self) -> usize {
        self.inner.accounts(SERVICE).len()
    }
    fn ops(&self, op: MemoryOp) -> Vec<String> {
        self.inner
            .calls()
            .into_iter()
            .filter(|(o, _)| *o == op)
            .map(|(_, a)| a)
            .collect()
    }
}

const SERVICE: &str = "happy-platform-mcp";

impl SecretStore for TestSecrets {
    fn get<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<Secret>, SecretStoreError>> {
        self.inner.get(service, account)
    }
    fn set<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
        value: Secret,
    ) -> BoxFut<'a, Result<(), SecretStoreError>> {
        self.clock.tick("setSecret");
        self.inner.set(service, account, value)
    }
    fn delete<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<bool, SecretStoreError>> {
        // Only credential references count; the store's health probe does not
        // (the JS fake had no probe).
        if !account.starts_with("keychain:instance/") {
            return self.inner.delete(service, account);
        }
        let n = {
            let mut d = self.deletes.lock().unwrap();
            *d += 1;
            *d
        };
        if *self.fail_delete_at.lock().unwrap() == Some(n) {
            return Box::pin(async { Err(SecretStoreError::Operation) });
        }
        self.inner.delete(service, account)
    }
}

/// `prompts(values)`: answers by name, defaults like the Jest helper.
struct FakePrompts {
    values: BTreeMap<String, Value>,
    kinds: BTreeSet<PromptKind>,
    calls: PromptLog,
    passwords: VecDeque<String>,
}

/// Every prompt asked, in order.
type PromptLog = Arc<Mutex<Vec<(PromptKind, PromptSpec)>>>;

fn prompts(values: Value) -> (Box<FakePrompts>, PromptLog) {
    prompts_of(
        values,
        &[
            PromptKind::Input,
            PromptKind::Password,
            PromptKind::Select,
            PromptKind::Confirm,
        ],
    )
}

fn prompts_of(values: Value, kinds: &[PromptKind]) -> (Box<FakePrompts>, PromptLog) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    (
        Box::new(FakePrompts {
            values: values
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            kinds: kinds.iter().copied().collect(),
            calls: calls.clone(),
            passwords: VecDeque::new(),
        }),
        calls,
    )
}

impl Prompter for FakePrompts {
    fn injected(&self) -> BTreeSet<PromptKind> {
        self.kinds.clone()
    }
    fn ask(&mut self, kind: PromptKind, spec: &PromptSpec) -> Result<Option<Value>, CodedError> {
        self.calls.lock().unwrap().push((kind, spec.clone()));
        let given = self.values.get(&spec.name).cloned();
        Ok(Some(match kind {
            PromptKind::Password => match self.passwords.pop_front() {
                Some(p) => json!(p),
                None => given.unwrap_or(json!("fixture-secret-value")),
            },
            PromptKind::Input => given.or(spec.default.clone()).unwrap_or(json!("")),
            PromptKind::Select => given
                .or(spec.default.clone())
                .or_else(|| spec.choices.first().map(|(_, v)| json!(v)))
                .unwrap_or(Value::Null),
            PromptKind::Confirm => given.or(spec.default.clone()).unwrap_or(json!(true)),
        }))
    }
}

fn count(calls: &PromptLog, kind: PromptKind) -> usize {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(k, _)| *k == kind)
        .count()
}

/// Probe answering with a fixed outcome.
struct FixedProbe(Option<u16>);
impl Probe for FixedProbe {
    fn probe<'a>(
        &'a self,
        _: &'a Object,
        _: &'a InstanceCredentialStore,
    ) -> BoxFut<'a, Result<(), ProbeError>> {
        let status = self.0;
        Box::pin(async move {
            match status {
                None => Ok(()),
                Some(s) => Err(ProbeError {
                    status: Some(s),
                    message: "request failed".into(),
                }),
            }
        })
    }
}

/// Token store whose every call fails (`broken` / `untouchableTokenStore`).
#[derive(Default)]
struct FailingTokenStore {
    calls: Mutex<usize>,
}
impl TokenStore for FailingTokenStore {
    fn get_refresh_token<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFut<'a, Result<Option<RefreshToken>, CodedError>> {
        *self.calls.lock().unwrap() += 1;
        Box::pin(async { Err(CodedError::plain("store locked")) })
    }
    fn set_refresh_token<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFut<'a, Result<(), CodedError>> {
        *self.calls.lock().unwrap() += 1;
        Box::pin(async { Err(CodedError::plain("store locked")) })
    }
    fn clear_refresh_token<'a>(&'a self, _: &'a str) -> BoxFut<'a, Result<(), CodedError>> {
        *self.calls.lock().unwrap() += 1;
        Box::pin(async { Err(CodedError::plain("store locked")) })
    }
}

const OS_USER: &str = "tester";

struct Harness {
    registry: Arc<dyn CliRegistry>,
    secrets: Arc<TestSecrets>,
    prompter: Box<dyn Prompter>,
    tty: bool,
    probe: Box<dyn Probe>,
    token_stores: Vec<(&'static str, Arc<dyn TokenStore>)>,
}

impl Harness {
    fn new(registry: Arc<dyn CliRegistry>, prompter: Box<dyn Prompter>) -> Self {
        Self {
            registry,
            secrets: TestSecrets::new(Arc::new(Clock::default())),
            prompter,
            tty: false,
            probe: Box::new(FixedProbe(None)),
            token_stores: vec![("OS keychain", Arc::new(InMemoryTokenStore::new()))],
        }
    }
    async fn run(self, argv: &[&str]) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut ctx = CliContext {
            registry: self.registry,
            // CR-13: the bind mismatch delay is never slept in tests.
            credentials: InstanceCredentialStore::new(self.secrets.clone())
                .with_mismatch_pause(Arc::new(|_| Box::pin(async {}))),
            prompter: self.prompter,
            tty: self.tty,
            probe: self.probe,
            token_stores: self.token_stores,
            os_user: OS_USER.into(),
            stdout: &mut out,
            stderr: &mut err,
        };
        let code = run_instance_cli(&args(argv), &mut ctx).await;
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }
}

fn no_prompts() -> Box<dyn Prompter> {
    prompts(json!({})).0
}

// --- runInstanceCli ----------------------------------------------------------

#[tokio::test]
async fn lists_redacted_metadata_and_prints_an_empty_registry_message() {
    let mut leaky = basic();
    leaky.insert("password".into(), json!("must-not-appear"));
    let (code, out, _) = Harness::new(FakeRegistry::with(vec![leaky]), no_prompts())
        .run(&["instance", "list"])
        .await;
    assert_eq!(code, 0);
    assert!(out.contains("dev"));
    assert!(!out.contains("must-not-appear"));
    let (code, out, _) = Harness::new(FakeRegistry::with(vec![]), no_prompts())
        .run(&["instance", "list"])
        .await;
    assert_eq!(code, 0);
    assert!(out.contains("No registered instances"));
}

#[tokio::test]
async fn credential_set_uses_a_masked_prompt_and_deterministic_reference() {
    let mut inst = basic();
    inst.remove("credentialRef");
    let registry = FakeRegistry::with(vec![inst]);
    let (p, calls) = prompts(json!({"secret": "fixture-secret-value", "confirmOrigin": true}));
    let h = Harness::new(registry.clone(), p);
    let secrets = h.secrets.clone();
    let (code, out, _) = h
        .run(&["instance", "credential", "set", "dev", "--type", "password"])
        .await;
    assert_eq!(code, 0);
    let pw = calls
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| *k == PromptKind::Password)
        .cloned()
        .unwrap();
    assert!(pw.1.mask);
    assert_eq!(
        secrets.value("keychain:instance/dev/password").as_deref(),
        Some("fixture-secret-value")
    );
    assert_eq!(
        registry.calls("update")[0][1]["credentialRef"],
        json!("keychain:instance/dev/password")
    );
    assert!(!out.contains("fixture-secret-value"));
}

const DEV_ORIGIN: &str = "https://dev.service-now.com";
const DEV_REF: &str = "keychain:instance/dev/password";

/// Seeds `dev`'s password, bound to `origin` when given (as the CLI writes it).
async fn seed_dev(secrets: &Arc<TestSecrets>, value: &str, origin: Option<&str>) {
    let store = InstanceCredentialStore::new(secrets.clone());
    store.restore_secret(DEV_REF, value, origin).await.unwrap();
}

async fn bound(secrets: &Arc<TestSecrets>) -> Option<String> {
    InstanceCredentialStore::new(secrets.clone())
        .bound_origin(DEV_REF)
        .await
        .unwrap()
}

/// M-A: `credential set` names the target origin before the secret prompt;
/// re-binding to a different origin needs a confirm that defaults to No.
#[tokio::test]
async fn credential_set_shows_origin_and_declined_rebind_changes_nothing() {
    let h = Harness::new(FakeRegistry::with(vec![basic()]), prompts(json!({})).0);
    let secrets = h.secrets.clone();
    seed_dev(&secrets, "old-secret", Some("https://old.service-now.com")).await;
    let (code, out, _) = h
        .run(&["instance", "credential", "set", "dev", "--type", "password"])
        .await;
    assert_eq!(code, 0);
    assert!(
        out.starts_with(&format!("{CREDENTIAL_ORIGIN_PREFIX}{DEV_ORIGIN}")),
        "{out}"
    );
    assert!(out.contains("Cancelled."));
    assert_eq!(secrets.value(DEV_REF).as_deref(), Some("old-secret"));
    assert_eq!(
        bound(&secrets).await.as_deref(),
        Some("https://old.service-now.com")
    );
}

#[tokio::test]
async fn credential_set_confirmed_rebind_and_same_origin_needs_no_confirm() {
    let (p, calls) = prompts(json!({"secret": "new-secret", "confirmOrigin": true}));
    let h = Harness::new(FakeRegistry::with(vec![basic()]), p);
    let secrets = h.secrets.clone();
    seed_dev(&secrets, "old-secret", Some("https://old.service-now.com")).await;
    let (code, _, _) = h
        .run(&["instance", "credential", "set", "dev", "--type", "password"])
        .await;
    assert_eq!(code, 0);
    let confirm = calls
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| *k == PromptKind::Confirm)
        .cloned()
        .unwrap();
    assert!(confirm.1.message.contains("https://old.service-now.com"));
    assert!(confirm.1.message.contains(DEV_ORIGIN));
    assert_eq!(bound(&secrets).await.as_deref(), Some(DEV_ORIGIN));

    // Already bound to the registered origin: no confirm.
    let (p, calls) = prompts(json!({"secret": "newer"}));
    let mut h = Harness::new(FakeRegistry::with(vec![basic()]), p);
    h.secrets = secrets.clone();
    let (code, _, _) = h
        .run(&["instance", "credential", "set", "dev", "--type", "password"])
        .await;
    assert_eq!(code, 0);
    assert_eq!(count(&calls, PromptKind::Confirm), 0);
    assert_eq!(secrets.value(DEV_REF).as_deref(), Some("newer"));
}

#[tokio::test]
async fn credential_set_non_tty_refuses_without_writing() {
    let h = Harness::new(
        FakeRegistry::with(vec![basic()]),
        prompts_of(json!({}), &[]).0,
    );
    let secrets = h.secrets.clone();
    let (code, _, err) = h
        .run(&["instance", "credential", "set", "dev", "--type", "password"])
        .await;
    assert_ne!(code, 0);
    assert!(err.contains("Non-interactive use requires"), "{err}");
    assert!(secrets.value(DEV_REF).is_none());
}

/// M-C/M-D: `credential bind` binds an unbound secret to the registered origin
/// after the secret is re-entered (masked) and the origin confirmed.
#[tokio::test]
async fn credential_bind_confirmed_binds_to_registered_origin() {
    let (p, calls) = prompts(json!({"confirm": true, "secret": "legacy"}));
    let h = Harness::new(FakeRegistry::with(vec![basic()]), p);
    let secrets = h.secrets.clone();
    seed_dev(&secrets, "legacy", None).await;
    let (code, out, _) = h.run(&["instance", "credential", "bind", "dev"]).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(count(&calls, PromptKind::Confirm), 1);
    assert_eq!(count(&calls, PromptKind::Password), 1);
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(k, s)| *k == PromptKind::Password && s.mask)
    );
    let store = InstanceCredentialStore::new(secrets.clone());
    assert_eq!(
        store
            .get_secret_for_origin(DEV_REF, DEV_ORIGIN)
            .await
            .unwrap()
            .expose(),
        "legacy"
    );
    assert_eq!(secrets.value(DEV_REF).as_deref(), Some("legacy"));
}

#[tokio::test]
async fn credential_bind_declined_or_bound_elsewhere_writes_nothing() {
    let h = Harness::new(
        FakeRegistry::with(vec![basic()]),
        prompts(json!({"secret": "legacy"})).0,
    );
    let secrets = h.secrets.clone();
    seed_dev(&secrets, "legacy", None).await;
    let (code, out, _) = h.run(&["instance", "credential", "bind", "dev"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("Cancelled."));
    assert_eq!(bound(&secrets).await, None);

    let mut h = Harness::new(
        FakeRegistry::with(vec![basic()]),
        prompts(json!({"confirm": true})).0,
    );
    h.secrets = secrets.clone();
    seed_dev(&secrets, "legacy", Some("https://old.service-now.com")).await;
    let (code, _, err) = h.run(&["instance", "credential", "bind", "dev"]).await;
    assert_ne!(code, 0);
    assert!(
        err.contains("already bound to https://old.service-now.com"),
        "{err}"
    );
    assert_eq!(
        bound(&secrets).await.as_deref(),
        Some("https://old.service-now.com")
    );
}

#[tokio::test]
async fn credential_bind_refuses_unknown_instance_missing_secret_and_non_tty() {
    let (code, _, err) = Harness::new(FakeRegistry::with(vec![]), no_prompts())
        .run(&["instance", "credential", "bind", "dev"])
        .await;
    assert_ne!(code, 0);
    assert!(err.contains("dev"), "{err}");
    let (code, _, err) = Harness::new(FakeRegistry::with(vec![basic()]), no_prompts())
        .run(&["instance", "credential", "bind", "dev"])
        .await;
    assert_ne!(code, 0);
    assert!(err.contains("No saved credential to bind"), "{err}");
    let h = Harness::new(
        FakeRegistry::with(vec![basic()]),
        prompts_of(json!({}), &[]).0,
    );
    let secrets = h.secrets.clone();
    seed_dev(&secrets, "legacy", None).await;
    let (code, _, err) = h.run(&["instance", "credential", "bind", "dev"]).await;
    assert_ne!(code, 0);
    assert!(err.contains("Non-interactive use requires"), "{err}");
    assert_eq!(bound(&secrets).await, None);
}

/// M-D: a wrong re-entered secret is refused before the confirm and writes
/// nothing, even with the confirm answered yes.
#[tokio::test]
async fn credential_bind_wrong_secret_refused_without_write() {
    let (p, calls) = prompts(json!({"confirm": true, "secret": "not-the-secret"}));
    let h = Harness::new(FakeRegistry::with(vec![basic()]), p);
    let secrets = h.secrets.clone();
    seed_dev(&secrets, "legacy", None).await;
    let (code, _, err) = h.run(&["instance", "credential", "bind", "dev"]).await;
    assert_ne!(code, 0);
    assert!(err.contains("does not match the saved credential"), "{err}");
    assert_eq!(count(&calls, PromptKind::Confirm), 0);
    assert_eq!(bound(&secrets).await, None);
    assert_eq!(secrets.value(DEV_REF).as_deref(), Some("legacy"));
}

/// M-D: a non-TTY caller that can only answer the confirm (pty-style `yes`)
/// cannot bind: the masked secret prompt is required.
#[tokio::test]
async fn credential_bind_confirm_alone_cannot_bind() {
    let (p, calls) = prompts_of(json!({"confirm": true}), &[PromptKind::Confirm]);
    let h = Harness::new(FakeRegistry::with(vec![basic()]), p);
    let secrets = h.secrets.clone();
    seed_dev(&secrets, "legacy", None).await;
    let (code, _, err) = h.run(&["instance", "credential", "bind", "dev"]).await;
    assert_ne!(code, 0);
    assert!(err.contains("Non-interactive use requires"), "{err}");
    assert_eq!(count(&calls, PromptKind::Confirm), 0);
    assert_eq!(bound(&secrets).await, None);
}

/// M-C: the Basic-auth probe reads only a secret bound to the instance
/// origin; unbound and mismatched secrets are refused and nothing connects.
#[tokio::test]
async fn basic_probe_refuses_unbound_and_mismatched_secrets_unsent() {
    use std::net::TcpListener;
    for binding in [None, Some("https://elsewhere.example")] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let memory = Arc::new(MemorySecretStore::new());
        let credentials = InstanceCredentialStore::new(memory);
        credentials
            .restore_secret(DEV_REF, "pw", binding)
            .await
            .unwrap();
        let instance = obj(json!({
            "name": "dev", "url": url, "authType": "basic", "username": "u",
            "credentialRef": DEV_REF
        }));
        let err = HttpProbe.probe(&instance, &credentials).await.unwrap_err();
        let want = if binding.is_none() {
            "not bound"
        } else {
            "elsewhere.example"
        };
        assert!(err.message.contains(want), "{}", err.message);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            listener.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "nothing may connect"
        );
    }
}

/// Review L-9 R3: the probe checks the binding MAC, not just the stored
/// origin. A planted plain or forged binding naming the right origin is
/// treated as unbound and nothing connects.
#[tokio::test]
async fn basic_probe_refuses_planted_binding_for_the_right_origin() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let forged = json!({"origin": url, "mac": "00".repeat(32)}).to_string();
    for planted in [url.clone(), forged] {
        let memory = Arc::new(MemorySecretStore::new());
        memory.insert(SERVICE_NAME, DEV_REF, "pw");
        memory.insert(
            SERVICE_NAME,
            &hpm_core::keychain::origin_binding_account(DEV_REF),
            &planted,
        );
        let credentials = InstanceCredentialStore::new(memory);
        let instance = obj(json!({
            "name": "dev", "url": url, "authType": "basic", "username": "u",
            "credentialRef": DEV_REF
        }));
        let err = HttpProbe.probe(&instance, &credentials).await.unwrap_err();
        assert!(err.message.contains("not bound"), "{}", err.message);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            listener.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "nothing may connect"
        );
    }
}

/// RH-4 (review L-9′): the OAuth probe reads the client secret and, for the
/// password grant, the password only when each is bound to the instance
/// origin. A planted (plain or forged) binding, an unbound secret and a
/// secret bound elsewhere are all refused and nothing reaches the token
/// endpoint.
#[tokio::test]
async fn oauth_probe_refuses_planted_unbound_and_mismatched_secrets_unsent() {
    use std::net::TcpListener;
    const SECRET_REF: &str = "keychain:instance/dev/client-secret";
    #[derive(Debug, Clone, Copy)]
    enum Case {
        PlantedPlain,
        PlantedForged,
        Unbound,
        Mismatched,
    }
    for grant in ["client_credentials", "password"] {
        // The credential under test; the other (password grant) is bound.
        let target = if grant == "password" {
            DEV_REF
        } else {
            SECRET_REF
        };
        for case in [
            Case::PlantedPlain,
            Case::PlantedForged,
            Case::Unbound,
            Case::Mismatched,
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let memory = Arc::new(MemorySecretStore::new());
            let credentials = InstanceCredentialStore::new(memory.clone());
            if grant == "password" {
                credentials
                    .restore_secret(SECRET_REF, "client-secret", Some(&url))
                    .await
                    .unwrap();
            }
            let binding = hpm_core::keychain::origin_binding_account(target);
            match case {
                Case::PlantedPlain | Case::PlantedForged => {
                    memory.insert(SERVICE_NAME, target, "pw");
                    let planted = match case {
                        Case::PlantedPlain => url.clone(),
                        _ => json!({"origin": url, "mac": "00".repeat(32)}).to_string(),
                    };
                    memory.insert(SERVICE_NAME, &binding, &planted);
                }
                Case::Unbound => memory.insert(SERVICE_NAME, target, "pw"),
                Case::Mismatched => credentials
                    .restore_secret(target, "pw", Some("https://elsewhere.example"))
                    .await
                    .unwrap(),
            }
            let mut refs = serde_json::Map::new();
            refs.insert("clientSecret".into(), json!(SECRET_REF));
            if grant == "password" {
                refs.insert("password".into(), json!(DEV_REF));
            }
            let instance = obj(json!({
                "name": "dev", "url": url, "authType": "oauth", "grantType": grant,
                "clientId": "client", "username": "u", "credentialRef": refs
            }));
            let err = HttpProbe.probe(&instance, &credentials).await.unwrap_err();
            let want = match case {
                Case::Mismatched => "elsewhere.example",
                _ => "not bound",
            };
            assert!(
                err.message.contains(want),
                "{grant} {case:?}: {}",
                err.message
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert_eq!(
                listener.accept().map(|_| ()).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock,
                "{grant} {case:?}: nothing may reach the token endpoint"
            );
        }
    }
}

fn add_answers(name: &str) -> Value {
    json!({"name": name, "url": "https://dev.service-now.com", "authType": "basic", "username": "developer"})
}

#[tokio::test]
async fn add_stores_credential_before_metadata_and_rolls_back_a_new_secret_on_registry_failure() {
    let clock = Arc::new(Clock::default());
    let registry = Arc::new(FakeRegistry {
        state: Mutex::new(vec![]),
        clock: Some(clock.clone()),
        ..Default::default()
    });
    *registry.fail_register.lock().unwrap() = Some(registry_error());
    let mut h = Harness::new(registry.clone(), prompts(add_answers("dev")).0);
    h.secrets = TestSecrets::new(clock.clone());
    let secrets = h.secrets.clone();
    let (code, _, err) = h.run(&["instance", "add"]).await;
    assert_eq!(code, 1);
    assert!(clock.position("setSecret").unwrap() < clock.position("register").unwrap());
    assert!(
        secrets
            .ops(MemoryOp::Delete)
            .contains(&"keychain:instance/dev/password".to_owned())
    );
    assert!(secrets.value("keychain:instance/dev/password").is_none());
    assert!(err.contains("REGISTRY_WRITE_FAILED"));
    assert!(err.contains(&registry_path_field()), "{err}");
}

#[tokio::test]
async fn preflights_duplicate_and_validation_before_writing_deterministic_secrets() {
    let clock = Arc::new(Clock::default());
    let registry = Arc::new(FakeRegistry {
        clock: Some(clock.clone()),
        ..Default::default()
    });
    let mut answers = add_answers("new-instance");
    answers["url"] = json!("https://new.service-now.com");
    let mut h = Harness::new(registry.clone(), prompts(answers).0);
    h.secrets = TestSecrets::new(clock.clone());
    let (code, _, _) = h.run(&["instance", "add"]).await;
    assert_eq!(code, 0);
    let validated = &registry.calls("validate")[0];
    assert_eq!(validated["name"], json!("new-instance"));
    assert_eq!(
        validated["credentialRef"],
        json!("keychain:instance/new-instance/password")
    );
    assert!(clock.position("validate").unwrap() < clock.position("setSecret").unwrap());
}

#[tokio::test]
async fn restores_an_overwritten_deterministic_secret_when_registration_fails() {
    let registry = FakeRegistry::with(vec![]);
    *registry.fail_register.lock().unwrap() = Some(registry_error());
    let h = Harness::new(registry, prompts(add_answers("dev")).0);
    h.secrets
        .put("keychain:instance/dev/password", "previous-secret");
    let secrets = h.secrets.clone();
    let (code, _, err) = h.run(&["instance", "add"]).await;
    assert_eq!(code, 1);
    assert_eq!(
        secrets.value("keychain:instance/dev/password").as_deref(),
        Some("previous-secret")
    );
    assert!(err.contains("REGISTRY_WRITE_FAILED"));
    assert!(err.contains(&registry_path_field()), "{err}");
}

#[tokio::test]
async fn reports_credential_rollback_failure_without_replacing_the_registration_cause() {
    let registry = FakeRegistry::with(vec![]);
    *registry.fail_register.lock().unwrap() = Some(registry_error());
    let h = Harness::new(registry, prompts(add_answers("new-instance")).0);
    // Delete 1 is the write's own CR-11 delete-before-create of the secret
    // item (binding items are not counted); delete 2 is the rollback's.
    *h.secrets.fail_delete_at.lock().unwrap() = Some(2);
    let (code, _, err) = h.run(&["instance", "add"]).await;
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("REGISTRY_WRITE_FAILED"), "{err}");
    assert!(err.contains(&registry_path_field()), "{err}");
    assert!(err.contains("rollback failed"), "{err}");
    assert!(!err.contains("fixture-secret-value"));
}

#[tokio::test]
async fn rejects_duplicate_names_before_prompting_for_or_writing_a_secret() {
    let (p, calls) = prompts(add_answers("dev"));
    let h = Harness::new(FakeRegistry::with(vec![basic()]), p);
    let secrets = h.secrets.clone();
    let (code, _, err) = h.run(&["instance", "add"]).await;
    assert_eq!(code, 1);
    assert!(secrets.ops(MemoryOp::Set).is_empty());
    assert_eq!(count(&calls, PromptKind::Password), 0);
    assert!(err.contains("already exists"));
}

#[tokio::test]
async fn rejects_secret_flags_before_prompts_or_output() {
    let (p, calls) = prompts(json!({}));
    let (code, out, err) = Harness::new(FakeRegistry::with(vec![basic()]), p)
        .run(&[
            "instance",
            "update",
            "dev",
            "--password",
            "fixture-secret-value",
        ])
        .await;
    assert_eq!(code, 2);
    assert!(calls.lock().unwrap().is_empty());
    assert!(!out.contains("fixture-secret-value"));
    assert!(!err.contains("fixture-secret-value"));
}

#[tokio::test]
async fn rejects_secret_assignment_before_parsing() {
    let cases: [&[&str]; 8] = [
        &["--password=fixture-secret-value"],
        &["password=fixture-secret-value"],
        &["--clientSecret=fixture-secret-value"],
        &["client-secret=fixture-secret-value"],
        &["client_secret=fixture-secret-value"],
        &["--CLIENT_SECRET=fixture-secret-value"],
        &["--type=fixture-secret-value"],
        &["--type", "fixture-secret-value"],
    ];
    for tokens in cases {
        let registry = FakeRegistry::with(vec![basic()]);
        let mut argv = vec!["instance", "credential", "set", "dev"];
        argv.extend_from_slice(tokens);
        let (code, out, err) = Harness::new(registry.clone(), no_prompts())
            .run(&argv)
            .await;
        assert_eq!(code, 2, "{tokens:?}");
        assert_eq!(err, SECRET_MSG);
        assert!(!out.contains("fixture-secret-value"));
        assert!(registry.calls("get").is_empty());
    }
}

#[tokio::test]
async fn unknown_credential_type_uses_constant_safe_usage_text() {
    let (code, _, err) = Harness::new(FakeRegistry::with(vec![basic()]), no_prompts())
        .run(&[
            "instance",
            "credential",
            "set",
            "dev",
            "--type",
            "fixture-secret-value",
        ])
        .await;
    assert_eq!(code, 2);
    assert_eq!(err, SECRET_MSG);
}

#[tokio::test]
async fn registry_write_failures_include_only_stable_code_and_resolved_path() {
    let registry = FakeRegistry::with(vec![]);
    *registry.fail_register.lock().unwrap() = Some(
        CodedError::new("REGISTRY_WRITE_FAILED", "fixture-secret-value")
            .with("path", "./fixture-registry.json")
            .with("secret", "fixture-secret-value")
            .with("arbitrary", "do-not-echo"),
    );
    let (code, _, err) = Harness::new(registry, prompts(add_answers("dev")).0)
        .run(&["instance", "add"])
        .await;
    assert_eq!(code, 1);
    assert!(err.contains("REGISTRY_WRITE_FAILED"));
    assert!(err.contains("fixture-registry.json"));
    assert!(!err.contains("fixture-secret-value"));
    assert!(!err.contains("arbitrary"));
}

#[tokio::test]
async fn unknown_instance_commands_use_constant_safe_usage_text() {
    let (code, _, err) = Harness::new(FakeRegistry::with(vec![]), no_prompts())
        .run(&["instance", "fixture-secret-value"])
        .await;
    assert_eq!(code, 2);
    assert!(!err.contains("fixture-secret-value"));
    assert!(err.contains("Usage:"));
}

#[tokio::test]
async fn remove_confirms_before_metadata_and_referenced_secret_deletion() {
    let registry = FakeRegistry::with(vec![basic()]);
    let h = Harness::new(registry.clone(), prompts(json!({"confirm": false})).0);
    let secrets = h.secrets.clone();
    let (code, _, _) = h.run(&["instance", "remove", "dev"]).await;
    assert_eq!(code, 0);
    assert!(registry.calls("remove").is_empty());
    assert!(secrets.ops(MemoryOp::Delete).is_empty());
}

#[tokio::test]
async fn test_reports_success_authentication_and_authorization_failures_distinctly() {
    let registry = FakeRegistry::with(vec![basic()]);
    let (code, out, _) = Harness::new(registry.clone(), no_prompts())
        .run(&["instance", "test", "dev"])
        .await;
    assert_eq!(code, 0);
    assert!(out.contains("succeeded"));
    for (status, label) in [(401, "Authentication"), (403, "Authorization")] {
        let mut h = Harness::new(registry.clone(), no_prompts());
        h.probe = Box::new(FixedProbe(Some(status)));
        let (code, _, err) = h.run(&["instance", "test", "dev"]).await;
        assert_eq!(code, 1);
        assert!(err.contains(label));
    }
}

#[tokio::test]
async fn remove_restores_deleted_secrets_and_leaves_metadata_when_a_later_deletion_fails() {
    let mut inst = basic();
    inst.insert(
        "credentialRef".into(),
        json!({"password": "keychain:instance/dev/password", "clientSecret": "keychain:instance/dev/client-secret"}),
    );
    inst.insert("authType".into(), json!("oauth"));
    inst.insert("grantType".into(), json!("password"));
    inst.insert("clientId".into(), json!("client"));
    let registry = FakeRegistry::with(vec![inst]);
    let h = Harness::new(registry.clone(), prompts(json!({"confirm": true})).0);
    h.secrets
        .put("keychain:instance/dev/password", "old-password");
    h.secrets
        .put("keychain:instance/dev/client-secret", "old-client-secret");
    *h.secrets.fail_delete_at.lock().unwrap() = Some(2);
    let secrets = h.secrets.clone();
    let (code, _, err) = h.run(&["instance", "remove", "dev"]).await;
    assert_eq!(code, 1);
    assert!(registry.calls("remove").is_empty());
    assert_eq!(
        secrets.value("keychain:instance/dev/password").as_deref(),
        Some("old-password")
    );
    assert!(err.contains("rollback"));
}

#[tokio::test]
async fn remove_restores_all_secrets_when_metadata_removal_fails() {
    let registry = FakeRegistry::with(vec![basic()]);
    *registry.fail_remove.lock().unwrap() = Some(registry_error());
    let h = Harness::new(registry, prompts(json!({"confirm": true})).0);
    h.secrets
        .put("keychain:instance/dev/password", "old-password");
    let secrets = h.secrets.clone();
    let (code, _, err) = h.run(&["instance", "remove", "dev"]).await;
    assert_eq!(code, 1);
    assert_eq!(
        secrets.value("keychain:instance/dev/password").as_deref(),
        Some("old-password")
    );
    assert!(err.contains("REGISTRY_WRITE_FAILED"));
    assert!(err.contains(&registry_path_field()), "{err}");
}

#[tokio::test]
async fn updates_only_allowed_metadata_flags_without_prompting_for_secrets() {
    let registry = FakeRegistry::with(vec![basic()]);
    let (p, calls) = prompts(json!({}));
    let (code, _, _) = Harness::new(registry.clone(), p)
        .run(&["instance", "update", "dev", "--description", "Updated"])
        .await;
    assert_eq!(code, 0);
    assert_eq!(
        registry.calls("update")[0],
        json!(["dev", {"description": "Updated"}])
    );
    assert_eq!(count(&calls, PromptKind::Password), 0);
}

#[tokio::test]
async fn rejects_auth_changes_before_registry_mutation_with_re_add_guidance() {
    for (flag, value) in [
        ("--auth-type", "oauth"),
        ("--grant-type", "client_credentials"),
    ] {
        let registry = FakeRegistry::with(vec![basic()]);
        let (code, _, err) = Harness::new(registry.clone(), no_prompts())
            .run(&["instance", "update", "dev", flag, value])
            .await;
        assert_eq!(code, 2);
        assert!(registry.calls("update").is_empty());
        assert!(
            err.to_lowercase()
                .contains("authentication changes require remove/re-add")
        );
    }
}

#[tokio::test]
async fn uses_the_inferred_password_grant_and_preserves_the_canonical_object() {
    let inst = obj(json!({
        "name": "inferred", "url": "https://inferred.service-now.com", "authType": "oauth",
        "clientId": "client", "username": "developer",
        "credentialRef": {"clientSecret": "keychain:instance/inferred/client-secret"}
    }));
    let expected = json!({"credentialRef": {
        "password": "keychain:instance/inferred/password",
        "clientSecret": "keychain:instance/inferred/client-secret"
    }});
    let registry = FakeRegistry::with(vec![inst]);
    let (code, _, _) = Harness::new(
        registry.clone(),
        prompts(json!({"secret": "new-password", "confirmOrigin": true})).0,
    )
    .run(&[
        "instance",
        "credential",
        "set",
        "inferred",
        "--type",
        "password",
    ])
    .await;
    assert_eq!(code, 0);
    assert_eq!(registry.calls("update")[0], json!(["inferred", expected]));
    let (mut p, _) = prompts(json!({"confirmOrigin": true}));
    p.passwords.push_back("new-client-secret".into());
    let (code, _, _) = Harness::new(registry.clone(), p)
        .run(&[
            "instance",
            "credential",
            "set",
            "inferred",
            "--type",
            "client-secret",
        ])
        .await;
    assert_eq!(code, 0);
    assert_eq!(registry.calls("update")[1], json!(["inferred", expected]));
}

const BAD_PORTS: [&str; 7] = ["12junk", "12.5", "1e2", "0x10", "+12", "-1", ""];

fn add_redirect(port: &str) -> Vec<&str> {
    vec![
        "instance",
        "add",
        "--name",
        "redirect",
        "--url",
        "https://redirect.service-now.com",
        "--auth-type",
        "oauth",
        "--grant-type",
        "authorization_code",
        "--client-id",
        "client",
        "--redirect-port",
        port,
    ]
}

#[tokio::test]
async fn rejects_invalid_add_redirect_port_before_registry_validation() {
    for port in BAD_PORTS {
        let registry = FakeRegistry::with(vec![]);
        let (_, _, err) = Harness::new(registry.clone(), no_prompts())
            .run(&add_redirect(port))
            .await;
        assert!(
            err.to_lowercase().contains("redirect-port"),
            "{port:?}: {err}"
        );
        assert!(registry.calls("validate").is_empty());
    }
}

#[tokio::test]
async fn normalizes_a_valid_add_redirect_port_before_validation_and_persistence() {
    let registry = FakeRegistry::with(vec![]);
    let (code, _, _) = Harness::new(registry.clone(), no_prompts())
        .run(&add_redirect("8455"))
        .await;
    assert_eq!(code, 0);
    assert_eq!(registry.calls("validate")[0]["redirectPort"], json!(8455));
    assert_eq!(
        registry.calls("register")[0][0]["redirectPort"],
        json!(8455)
    );
}

#[tokio::test]
async fn rejects_invalid_update_redirect_port_before_registry_update() {
    for port in BAD_PORTS {
        let registry = FakeRegistry::with(vec![basic()]);
        let (_, _, err) = Harness::new(registry.clone(), no_prompts())
            .run(&["instance", "update", "dev", "--redirect-port", port])
            .await;
        assert!(
            err.to_lowercase().contains("redirect-port"),
            "{port:?}: {err}"
        );
        assert!(registry.calls("update").is_empty());
    }
}

#[tokio::test]
async fn normalizes_a_valid_update_redirect_port_before_registry_update() {
    let registry = FakeRegistry::with(vec![basic()]);
    let (code, _, _) = Harness::new(registry.clone(), no_prompts())
        .run(&["instance", "update", "dev", "--redirect-port", "8455"])
        .await;
    assert_eq!(code, 0);
    assert_eq!(
        registry.calls("update")[0],
        json!(["dev", {"redirectPort": 8455}])
    );
}

#[tokio::test]
async fn unknown_commands_return_usage_code_2() {
    let (code, _, err) = Harness::new(FakeRegistry::with(vec![]), no_prompts())
        .run(&["instance", "wat"])
        .await;
    assert_eq!(code, 2);
    assert!(err.contains("Usage:"));
}

// --- migrate -------------------------------------------------------------------

fn migrate_registry(raw: Value, read: &Path, write: &Path) -> Arc<FakeRegistry> {
    let r = FakeRegistry {
        read_path: read.into(),
        write_path: write.into(),
        ..Default::default()
    };
    *r.raw.lock().unwrap() = raw;
    Arc::new(r)
}

fn one_plaintext() -> Value {
    json!({"instances": [{"name": "dev", "url": "https://dev.service-now.com", "username": "developer", "password": "fixture-password"}]})
}

#[tokio::test]
async fn migrate_stores_plaintext_credentials_as_refs_and_preserves_document_properties() {
    let legacy = json!({
        "version": 1,
        "docs": {"localIndexEnabled": true},
        "custom": {"keep": true},
        "instances": [
            {"name": "dev", "url": "https://dev.service-now.com", "username": "developer", "password": "fixture-password"},
            {"name": "oauth", "url": "https://oauth.service-now.com", "authType": "oauth", "grantType": "password", "clientId": "client", "username": "developer", "password": "fixture-password-2", "clientSecret": "fixture-client-secret"}
        ]
    });
    let registry = migrate_registry(
        legacy,
        Path::new("/tmp/hpm-legacy-src.json"),
        Path::new("/tmp/happy-user-registry.json"),
    );
    let h = Harness::new(registry.clone(), prompts(json!({"confirm": true})).0);
    let secrets = h.secrets.clone();
    let (code, out, _) = h.run(&["instance", "migrate"]).await;
    assert_eq!(code, 0);
    let written = registry.written.lock().unwrap().clone().unwrap();
    assert_eq!(written["docs"], json!({"localIndexEnabled": true}));
    assert_eq!(written["custom"], json!({"keep": true}));
    assert_eq!(
        written["instances"][0]["credentialRef"],
        json!("keychain:instance/dev/password")
    );
    assert!(written["instances"][0].get("password").is_none());
    assert_eq!(
        written["instances"][1]["credentialRef"],
        json!({"password": "keychain:instance/oauth/password", "clientSecret": "keychain:instance/oauth/client-secret"})
    );
    // Three secrets, each with its origin binding written alongside.
    assert_eq!(secrets.count(), 6);
    // Migrate binds each secret to its own entry's origin with a valid MAC.
    let item = secrets
        .value("keychain:instance-origin/dev/password")
        .unwrap();
    assert_eq!(
        hpm_core::keychain::verified_binding_origin(
            "keychain:instance/dev/password",
            &secrets.value("keychain:instance/dev/password").unwrap(),
            &item,
        )
        .as_deref(),
        Some("https://dev.service-now.com")
    );
    assert!(!out.contains("fixture-password"));
    assert!(!out.contains("fixture-client-secret"));
}

#[tokio::test]
async fn migrate_refuses_same_file_plaintext_source_before_keychain_writes() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = dir.path().join("instances.json");
    let source = serde_json::to_string_pretty(&json!({"version": 1, "instances": [
        {"name": "dev", "url": "https://dev.service-now.com", "username": "developer", "password": "fixture-password"}
    ]}))
    .unwrap();
    std::fs::write(&source_path, &source).unwrap();
    let registry = Arc::new(InstanceRegistry::at_paths(
        &source_path,
        &source_path,
        Env::from_pairs(&[]),
    ));
    let (p, calls) = prompts(json!({"confirm": true}));
    let h = Harness::new(registry, p);
    let secrets = h.secrets.clone();
    let (code, _, err) = h.run(&["instance", "migrate"]).await;
    assert_eq!(code, 2);
    assert_eq!(std::fs::read_to_string(&source_path).unwrap(), source);
    assert!(secrets.ops(MemoryOp::Set).is_empty());
    assert_eq!(count(&calls, PromptKind::Confirm), 0);
    assert!(err.contains("same file"));
}

#[cfg(unix)]
#[tokio::test]
async fn migrate_refuses_symlink_aliases_before_keychain_writes() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("instances.json");
    let alias = dir.path().join("alias.json");
    std::fs::write(&source, "{}").unwrap();
    std::os::unix::fs::symlink(&source, &alias).unwrap();
    let registry = migrate_registry(one_plaintext(), &source, &alias);
    let h = Harness::new(registry.clone(), prompts(json!({"confirm": true})).0);
    let secrets = h.secrets.clone();
    let (code, _, _) = h.run(&["instance", "migrate"]).await;
    assert_eq!(code, 2);
    assert!(secrets.ops(MemoryOp::Set).is_empty());
    assert!(registry.calls("write").is_empty());
}

#[tokio::test]
async fn migrate_allows_a_genuinely_nonexistent_destination_after_canonicalizing_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let registry = migrate_registry(
        one_plaintext(),
        &dir.path().join("legacy.json"),
        &dir.path().join("nested/instances.json"),
    );
    let h = Harness::new(registry.clone(), prompts(json!({"confirm": true})).0);
    let secrets = h.secrets.clone();
    let (code, _, _) = h.run(&["instance", "migrate"]).await;
    assert_eq!(code, 0);
    assert_eq!(
        secrets.value("keychain:instance/dev/password").as_deref(),
        Some("fixture-password")
    );
    assert_eq!(registry.calls("write").len(), 1);
}

#[tokio::test]
async fn migrate_preserves_missing_target_component_order_during_identity_comparison() {
    let dir = tempfile::tempdir().unwrap();
    let registry = migrate_registry(
        one_plaintext(),
        &dir.path().join("target/sub"),
        &dir.path().join("sub/target"),
    );
    let h = Harness::new(registry.clone(), prompts(json!({"confirm": true})).0);
    let secrets = h.secrets.clone();
    let (code, _, _) = h.run(&["instance", "migrate"]).await;
    assert_eq!(code, 0);
    assert_eq!(
        secrets.value("keychain:instance/dev/password").as_deref(),
        Some("fixture-password")
    );
    assert_eq!(registry.calls("write").len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn migrate_refuses_permission_errors_while_resolving_identity() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let read = locked.join("inner/plaintext-source.json");
    let write = dir.path().join("user-target.json");
    let registry = migrate_registry(one_plaintext(), &read, &write);
    let h = Harness::new(registry.clone(), prompts(json!({"confirm": true})).0);
    let secrets = h.secrets.clone();
    let (code, _, err) = h.run(&["instance", "migrate"]).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    if std::fs::metadata(locked.join(".")).is_ok() && unsafe_is_root() {
        return; // root bypasses permission bits
    }
    assert_eq!(code, 2);
    assert!(err.contains("MIGRATION_SOURCE_IDENTITY_FAILED"));
    assert!(err.contains(read.to_str().unwrap()));
    assert!(err.contains(write.to_str().unwrap()));
    assert!(!err.contains("fixture-password"));
    assert!(secrets.ops(MemoryOp::Set).is_empty());
    assert!(registry.calls("write").is_empty());
}

#[cfg(unix)]
fn unsafe_is_root() -> bool {
    std::env::var("USER").is_ok_and(|u| u == "root")
}

#[tokio::test]
async fn migrate_rolls_back_only_newly_created_secrets_when_registry_write_fails() {
    let dir = tempfile::tempdir().unwrap();
    let registry = migrate_registry(
        one_plaintext(),
        &dir.path().join("a.json"),
        &dir.path().join("b.json"),
    );
    *registry.fail_write.lock().unwrap() = Some(CodedError::plain("write failed"));
    let h = Harness::new(registry, prompts(json!({"confirm": true})).0);
    let secrets = h.secrets.clone();
    let (code, _, err) = h.run(&["instance", "migrate"]).await;
    assert_eq!(code, 1);
    assert_eq!(secrets.count(), 0);
    assert!(err.contains("write failed"));
}

#[tokio::test]
async fn migration_rejects_unsafe_legacy_shapes_before_keychain_writes() {
    let cases = [
        json!({"authType": "oauth", "grantType": "authorization_code", "clientId": "client", "clientSecret": "legacy-secret"}),
        json!({"authType": "oauth", "grantType": "password", "clientId": "client", "username": "developer", "password": "legacy-password"}),
        json!({"authType": "basic", "username": "developer", "password": "legacy-password", "token": "unexpected-secret"}),
    ];
    for fields in cases {
        let mut inst = obj(json!({"name": "dev", "url": "https://dev.service-now.com"}));
        inst.extend(obj(fields));
        let dir = tempfile::tempdir().unwrap();
        let registry = migrate_registry(
            json!({"docs": {"keep": true}, "instances": [inst]}),
            &dir.path().join("a.json"),
            &dir.path().join("b.json"),
        );
        let h = Harness::new(registry.clone(), prompts(json!({"confirm": true})).0);
        let secrets = h.secrets.clone();
        let (code, _, err) = h.run(&["instance", "migrate"]).await;
        assert_eq!(code, 1);
        assert!(secrets.ops(MemoryOp::Set).is_empty());
        assert!(registry.calls("write").is_empty());
        assert!(!err.contains("legacy-password"));
        assert!(!err.contains("legacy-secret"));
    }
}

// --- non-TTY -------------------------------------------------------------------

#[tokio::test]
async fn non_tty_real_prompts_return_guidance_without_invoking_inquirer() {
    let (p, calls) = prompts_of(json!({}), &[]);
    let (code, _, err) = Harness::new(FakeRegistry::with(vec![]), p)
        .run(&[
            "instance",
            "add",
            "--name",
            "dev",
            "--url",
            "https://dev.service-now.com",
            "--username",
            "developer",
        ])
        .await;
    assert_eq!(code, 2);
    assert!(err.to_lowercase().contains("non-interactive"));
    assert!(err.contains("masked prompt"));
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn fully_specified_public_authorization_code_add_runs_without_a_tty() {
    let registry = FakeRegistry::with(vec![]);
    let h = Harness::new(registry.clone(), prompts_of(json!({}), &[]).0);
    let secrets = h.secrets.clone();
    let (code, _, err) = h
        .run(&[
            "instance",
            "add",
            "--name",
            "public",
            "--url",
            "https://public.service-now.com",
            "--auth-type",
            "oauth",
            "--grant-type",
            "authorization_code",
            "--client-id",
            "public-client",
            "--scope",
            "openid",
            "--authorize-url",
            "https://public.service-now.com/authorize",
            "--token-url",
            "https://public.service-now.com/token",
            "--callback-path",
            "/oauth/callback",
            "--description",
            "public client",
        ])
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(registry.calls("register").len(), 1);
    assert!(secrets.ops(MemoryOp::Set).is_empty());
}

#[tokio::test]
async fn partial_prompt_injection_cannot_invoke_defaults_in_a_non_tty() {
    let cases: [(&[&str], PromptKind); 4] = [
        (&["instance", "add"], PromptKind::Password),
        (&["instance", "credential", "set", "dev"], PromptKind::Input),
        (
            &["instance", "credential", "set", "dev", "--type", "password"],
            PromptKind::Input,
        ),
        (&["instance", "remove", "dev"], PromptKind::Input),
    ];
    for (argv, injected) in cases {
        let (p, calls) = prompts_of(json!({}), &[injected]);
        let (code, _, _) = Harness::new(FakeRegistry::with(vec![basic()]), p)
            .run(argv)
            .await;
        assert_eq!(code, 2, "{argv:?}");
        assert!(calls.lock().unwrap().is_empty(), "{argv:?}");
    }
}

// --- OAuth refresh-token identity cleanup --------------------------------------

fn legacy_account() -> String {
    format!("{OS_USER}@code")
}

fn account_of(instance: &Object) -> String {
    oauth_refresh_token_account(instance, OS_USER).unwrap()
}

async fn seeded(entries: &[(&str, &str)]) -> Arc<InMemoryTokenStore> {
    let s = Arc::new(InMemoryTokenStore::new());
    for (a, t) in entries {
        s.set_refresh_token(a, t).await.unwrap();
    }
    s
}

#[tokio::test]
async fn remove_clears_identity_and_legacy_keys_from_both_stores_leaving_other_identities() {
    let current = account_of(&code_instance());
    let mut other_inst = code_instance();
    other_inst.insert("url".into(), json!("https://other.service-now.com"));
    let other = account_of(&other_inst);
    let legacy = legacy_account();
    let seed = [
        (current.as_str(), "rt-current"),
        (legacy.as_str(), "rt-legacy"),
        (other.as_str(), "rt-other"),
    ];
    let keychain = seeded(&seed).await;
    let file = seeded(&seed).await;
    let registry = FakeRegistry::with(vec![code_instance()]);
    let mut h = Harness::new(registry.clone(), prompts(json!({"confirm": true})).0);
    h.token_stores = vec![
        ("OS keychain", keychain.clone()),
        ("file token store", file.clone()),
    ];
    let (code, out, _) = h.run(&["instance", "remove", "code"]).await;
    assert_eq!(code, 0);
    assert_eq!(registry.calls("remove"), vec![json!("code")]);
    for store in [&keychain, &file] {
        assert_eq!(store.get_refresh_token(&current).await.unwrap(), None);
        assert_eq!(store.get_refresh_token(&legacy).await.unwrap(), None);
        assert_eq!(
            store
                .get_refresh_token(&other)
                .await
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("rt-other")
        );
    }
    assert!(out.contains("OS keychain, file token store"));
}

#[cfg(unix)]
#[tokio::test]
async fn remove_deletes_a_file_store_token_with_default_cli_wiring() {
    let home = tempfile::tempdir().unwrap();
    let env = Env::from_pairs(&[
        ("XDG_CONFIG_HOME", home.path().to_str().unwrap()),
        ("HOME", home.path().to_str().unwrap()),
    ]);
    let token_dir = default_token_dir(&env);
    let seeded_store = FileTokenStore::new(&token_dir).unwrap();
    let account = account_of(&code_instance());
    seeded_store
        .set_refresh_token(&account, "rt-left-behind")
        .await
        .unwrap();
    let registry_path = home.path().join("instances.json");
    let registry = Arc::new(InstanceRegistry::at_paths(
        &registry_path,
        &registry_path,
        env.clone(),
    ));
    registry
        .register(&Value::Object(code_instance()), RegisterOptions::default())
        .unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
    let mut ctx = CliContext::from_env(&env, secrets, &mut out, &mut err);
    ctx.registry = registry.clone();
    ctx.prompter = prompts(json!({"confirm": true})).0;
    ctx.os_user = OS_USER.into();
    assert_eq!(
        run_instance_cli(&args(&["instance", "remove", "code"]), &mut ctx).await,
        0
    );
    assert_eq!(
        FileTokenStore::new(&token_dir)
            .unwrap()
            .get_refresh_token(&account)
            .await
            .unwrap(),
        None
    );
    assert!(registry.list().unwrap().is_empty());
}

#[tokio::test]
async fn remove_fails_closed_before_deleting_anything_when_a_store_cannot_be_cleaned() {
    for failing in ["keychain", "file"] {
        let registry = FakeRegistry::with(vec![code_instance()]);
        let broken: Arc<dyn TokenStore> = Arc::new(FailingTokenStore::default());
        let healthy: Arc<dyn TokenStore> = seeded(&[]).await;
        let mut h = Harness::new(registry.clone(), prompts(json!({"confirm": true})).0);
        h.token_stores = if failing == "keychain" {
            vec![("OS keychain", broken), ("file token store", healthy)]
        } else {
            vec![("OS keychain", healthy), ("file token store", broken)]
        };
        let secrets = h.secrets.clone();
        let (code, _, _) = h.run(&["instance", "remove", "code"]).await;
        assert_eq!(code, 1, "{failing}");
        assert!(registry.calls("remove").is_empty());
        assert!(secrets.ops(MemoryOp::Delete).is_empty());
    }
}

#[tokio::test]
async fn locked_keychain_fails_closed_with_zero_registry_mutation() {
    let cases: [&[&str]; 2] = [
        &["instance", "remove", "code"],
        &[
            "instance",
            "update",
            "code",
            "--client-id",
            "rotated-client",
        ],
    ];
    for argv in cases {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("instances.json");
        let registry = Arc::new(InstanceRegistry::at_paths(
            &registry_path,
            &registry_path,
            Env::from_pairs(&[]),
        ));
        registry
            .register(&Value::Object(code_instance()), RegisterOptions::default())
            .unwrap();
        let before = std::fs::read(&registry_path).unwrap();
        let locked = Arc::new(MemorySecretStore::new());
        for _ in 0..4 {
            locked.fail_next(MemoryOp::Delete);
            locked.fail_next(MemoryOp::Get);
        }
        let file = FileTokenStore::new(&dir.path().join("tokens")).unwrap();
        let mut h = Harness::new(registry, prompts(json!({"confirm": true})).0);
        h.token_stores = vec![
            ("OS keychain", Arc::new(KeychainTokenStore::new(locked))),
            ("file token store", Arc::new(file)),
        ];
        let secrets = h.secrets.clone();
        let (code, _, _) = h.run(argv).await;
        assert_eq!(code, 1, "{argv:?}");
        assert_eq!(std::fs::read(&registry_path).unwrap(), before);
        assert!(secrets.ops(MemoryOp::Delete).is_empty());
    }
}

#[tokio::test]
async fn update_clears_the_previous_identity_from_both_stores_when_it_changes() {
    let cases: [&[&str]; 4] = [
        &["--client-id", "rotated-client"],
        &["--url", "https://moved.service-now.com"],
        &[
            "--token-url",
            "https://code.service-now.com/alternate_token.do",
        ],
        &[
            "--authorize-url",
            "https://code.service-now.com/alternate_auth.do",
        ],
    ];
    for flags in cases {
        let previous = account_of(&code_instance());
        let legacy = legacy_account();
        let seed = [
            (previous.as_str(), "rt-previous"),
            (legacy.as_str(), "rt-legacy"),
        ];
        let keychain = seeded(&seed).await;
        let file = seeded(&seed).await;
        let registry = FakeRegistry::with(vec![code_instance()]);
        let mut h = Harness::new(registry.clone(), no_prompts());
        h.token_stores = vec![
            ("OS keychain", keychain.clone()),
            ("file token store", file.clone()),
        ];
        let mut argv = vec!["instance", "update", "code"];
        argv.extend_from_slice(flags);
        let (code, out, err) = h.run(&argv).await;
        assert_eq!(code, 0, "{flags:?}: {err}");
        let updated = registry.state.lock().unwrap()[0].clone();
        assert_ne!(account_of(&updated), previous);
        for store in [&keychain, &file] {
            assert_eq!(store.get_refresh_token(&previous).await.unwrap(), None);
            assert_eq!(store.get_refresh_token(&legacy).await.unwrap(), None);
        }
        assert!(out.contains("OS keychain, file token store"));
    }
}

#[tokio::test]
async fn update_fails_closed_before_changing_the_registry_when_refresh_token_cleanup_fails() {
    let registry = FakeRegistry::with(vec![code_instance()]);
    let broken = Arc::new(FailingTokenStore::default());
    let mut h = Harness::new(registry.clone(), no_prompts());
    h.token_stores = vec![
        ("OS keychain", seeded(&[]).await),
        ("file token store", broken.clone()),
    ];
    let (code, _, _) = h
        .run(&[
            "instance",
            "update",
            "code",
            "--client-id",
            "rotated-client",
        ])
        .await;
    assert_eq!(code, 1);
    assert!(*broken.calls.lock().unwrap() > 0);
    assert!(registry.calls("update").is_empty());
    assert_eq!(
        registry.state.lock().unwrap()[0]["clientId"],
        json!("public-client")
    );
}

#[tokio::test]
async fn metadata_only_updates_and_basic_instances_never_touch_either_refresh_token_store() {
    let keychain = Arc::new(FailingTokenStore::default());
    let file = Arc::new(FailingTokenStore::default());
    let stores = || -> Vec<(&'static str, Arc<dyn TokenStore>)> {
        vec![
            ("OS keychain", keychain.clone()),
            ("file token store", file.clone()),
        ]
    };
    let mut h = Harness::new(FakeRegistry::with(vec![code_instance()]), no_prompts());
    h.token_stores = stores();
    assert_eq!(
        h.run(&["instance", "update", "code", "--description", "docs only"])
            .await
            .0,
        0
    );
    let mut h = Harness::new(
        FakeRegistry::with(vec![basic()]),
        prompts(json!({"confirm": true})).0,
    );
    h.token_stores = stores();
    assert_eq!(h.run(&["instance", "remove", "dev"]).await.0, 0);
    assert_eq!(*keychain.calls.lock().unwrap(), 0);
    assert_eq!(*file.calls.lock().unwrap(), 0);
}

// --- cli.test.js -----------------------------------------------------------------

fn route_of(argv: &[&str]) -> (Dispatch, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let d = route(&args(argv), &mut out, &mut err);
    (
        d,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn dispatch_rejects_secret_shaped_argv_before_unknown_command_formatting() {
    let (d, out, err) = route_of(&["--client-secret=token-value"]);
    assert_eq!(d, Dispatch::Exit(2));
    assert!(!err.contains("token-value"));
    assert!(!out.contains("token-value"));
}

#[test]
fn dispatch_rejects_secret_assignments_before_parsing() {
    for token in [
        "--password=fixture-secret-value",
        "password=fixture-secret-value",
        "--clientSecret=fixture-secret-value",
        "client-secret=fixture-secret-value",
        "client_secret=fixture-secret-value",
        "--CLIENT_SECRET=fixture-secret-value",
    ] {
        let (d, out, err) = route_of(&[token]);
        assert_eq!(d, Dispatch::Exit(2));
        assert_eq!(err, SECRET_MSG);
        assert_eq!(out, "");
    }
}

#[test]
fn dispatch_keeps_unknown_command_output_free_of_argv_tokens() {
    let (d, _, err) = route_of(&["fixture-secret-value"]);
    assert_eq!(d, Dispatch::Exit(2));
    assert_eq!(err, "Unknown command. Use --help for usage.\n");
}

#[test]
fn help_prints_usage_without_starting_stdio() {
    let (d, out, err) = route_of(&["--help"]);
    assert_eq!(d, Dispatch::Exit(0));
    assert_eq!(err, "");
    assert!(out.contains("Usage:"));
    // CR-2: the 6.2.0 USAGE bytes, then exactly the 7.0 bind line.
    assert_eq!(out, format!("{USAGE}\n{BIND_USAGE_LINE}\n"));
}

#[tokio::test]
async fn instance_help_names_credential_bind_after_the_6_2_0_usage() {
    let (code, out, err) = Harness::new(FakeRegistry::with(vec![]), no_prompts())
        .run(&["instance", "--help"])
        .await;
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(out, format!("{USAGE}\n{BIND_USAGE_LINE}\n"));
    assert!(BIND_USAGE_LINE.contains("instance credential bind <name>"));
}

#[test]
fn dispatch_delegates_server_startup_arguments_to_stdio() {
    assert_eq!(route_of(&[]).0, Dispatch::Serve);
    assert_eq!(route_of(&["--docs-only"]).0, Dispatch::Serve);
    assert_eq!(route_of(&["--help"]).0, Dispatch::Exit(0));
}

// --- security review fixes (p2/review-w2-a.md) -------------------------------

/// L5: even raw (unredacted) registry entries never reach `instance list` output,
/// including the index >= 1 entries that go through the 6.x `{}` quirk.
#[tokio::test]
async fn list_never_prints_planted_secrets_from_a_raw_registry() {
    let mut first = basic();
    first.insert("password".into(), json!("planted-password-0"));
    first.insert("clientSecret".into(), json!("planted-secret-0"));
    let mut second = code_instance();
    second.insert("password".into(), json!("planted-password-1"));
    second.insert("clientSecret".into(), json!("planted-secret-1"));
    let (code, out, err) = Harness::new(FakeRegistry::with(vec![first, second]), no_prompts())
        .run(&["instance", "list"])
        .await;
    assert_eq!(code, 0);
    for text in [&out, &err] {
        assert!(!text.contains("planted"), "{text}");
        assert!(!text.contains("\"password\""), "{text}");
        assert!(!text.contains("\"clientSecret\""), "{text}");
    }
}

fn failing_store() -> Arc<dyn SecretStore> {
    Arc::new(LazySecretStore::new(Box::new(|| {
        Err(SecretStoreError::Unavailable("no D-Bus session".into()))
    })))
}

/// M3: help, list and usage errors never open the keychain.
#[tokio::test]
async fn help_and_list_succeed_when_the_keychain_cannot_be_opened() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("instances.json");
    let registry = registry.to_str().unwrap();
    let env = Env::from_pairs(&[
        ("HAPPY_CONFIG_PATH", registry),
        ("HOME", tmp.path().to_str().unwrap()),
        ("XDG_CONFIG_HOME", tmp.path().to_str().unwrap()),
    ]);
    for (argv, want) in [
        (vec!["instance", "--help"], 0),
        (vec!["instance", "list"], 0),
        (vec!["instance", "bogus"], 2),
    ] {
        let argv = args(&argv);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(matches!(
            route(&argv, &mut out, &mut err),
            Dispatch::Instance
        ));
        let mut ctx = CliContext::from_env(&env, failing_store(), &mut out, &mut err);
        ctx.prompter = no_prompts();
        let code = run_instance_cli(&argv, &mut ctx).await;
        drop(ctx);
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, want, "{argv:?}: {err}");
        assert!(!err.contains("keychain unavailable"), "{argv:?}: {err}");
    }
}

/// M3: a credential operation still fails closed with the init error.
#[tokio::test]
async fn lazy_store_surfaces_init_failure_from_the_operation() {
    let store = failing_store();
    let e = store
        .get(SERVICE_NAME, "keychain:instance/dev/password")
        .await;
    assert!(matches!(e, Err(SecretStoreError::Unavailable(_))));
    assert!(store.delete(SERVICE_NAME, "x").await.is_err());
}

/// M1: a 307 from the token endpoint is a failure and is never followed, so the
/// client secret is never re-sent to the `Location` origin.
#[tokio::test]
async fn probe_never_follows_a_token_endpoint_redirect() {
    use std::io::{Read, Write as _};
    use std::net::TcpListener;
    let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
    elsewhere.set_nonblocking(true).unwrap();
    let elsewhere_port = elsewhere.local_addr().unwrap().port();
    let token = TcpListener::bind("127.0.0.1:0").unwrap();
    let token_port = token.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        // Bounded accept: a probe that refuses before sending fails the test
        // instead of hanging it.
        token.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut conn = loop {
            match token.accept() {
                Ok((conn, _)) => break conn,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the probe never reached the token endpoint"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        conn.set_nonblocking(false).unwrap();
        let mut buf = [0u8; 4096];
        let _ = conn.read(&mut buf).unwrap();
        let response = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{elsewhere_port}/oauth_token.do\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        conn.write_all(response.as_bytes()).unwrap();
    });

    let memory = Arc::new(MemorySecretStore::new());
    memory.insert(
        SERVICE_NAME,
        "keychain:instance/dev/client-secret",
        "planted-client-secret",
    );
    let credentials = InstanceCredentialStore::new(memory);
    // Saved for this instance, so bound to its origin (CLI rule).
    credentials
        .bind_origin(
            "keychain:instance/dev/client-secret",
            &format!("http://127.0.0.1:{token_port}"),
            "planted-client-secret",
        )
        .await
        .unwrap();
    let instance = obj(json!({
        "name": "dev", "url": format!("http://127.0.0.1:{token_port}"), "authType": "oauth",
        "grantType": "client_credentials", "clientId": "client",
        "credentialRef": {"clientSecret": "keychain:instance/dev/client-secret"}
    }));
    let err = HttpProbe
        .probe(&instance, &credentials)
        .await
        .expect_err("a redirect must fail the probe");
    server.join().unwrap();
    assert_eq!(err.status, Some(307));
    assert_eq!(err.message, "OAuth token request failed with status 307");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(
        elsewhere.accept().map(|_| ()).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "the redirect target must receive no connection"
    );
}

// --- CR-3: SN-Register-Instance success through the composed server ---------

struct NoProgress;
impl hpm_types::context::ProgressSink for NoProgress {
    fn emit(&self, _: f64, _: Option<f64>, _: Option<String>) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
}

/// CR-3: the single-user composition (`SingleUser`, as the binaries build it)
/// registers an instance whose origin-bound credential is in a fake secret
/// store: the registry file gets exactly the non-secret metadata and the
/// credential reference, and the next call (`SN-Set-Instance`) selects the
/// new instance. The only `AdmissionKey` mint in
/// this test binary.
#[tokio::test(flavor = "multi_thread")]
async fn register_instance_succeeds_through_the_server_with_a_bound_credential() {
    use hpm_core::client::ClientDeps;
    use hpm_core::config::ConfigManager;
    use hpm_core::registry::InstanceRegistry;
    use hpm_types::context::{AdmissionInput, AdmissionKey, ProtocolVersion, TrustedRoot};
    use hpm_types::dispatch::DispatchOutcome;

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("instances.json");
    std::fs::write(
        &file,
        json!({"version": 1, "instances": [
            {"name": "prod", "url": "https://prod.service-now.com", "authType": "oauth",
             "grantType": "authorization_code", "clientId": "c", "default": true}]})
        .to_string(),
    )
    .unwrap();
    let home = dir.path().join("home");
    let env = Env::from_pairs(&[
        ("HOME", home.to_str().unwrap()),
        ("XDG_CONFIG_HOME", home.to_str().unwrap()),
        ("HAPPY_CONFIG_PATH", file.to_str().unwrap()),
        ("SERVICENOW_TOKEN_STORE", "file"),
    ]);
    let config = Arc::new(ConfigManager::new(
        Arc::new(InstanceRegistry::at_paths(&file, &file, env.clone())),
        env,
    ));
    let memory = Arc::new(MemorySecretStore::new());
    InstanceCredentialStore::new(memory.clone())
        .set_secret(DEV_REF, "fixture-secret-value", DEV_ORIGIN)
        .await
        .unwrap();
    let key = AdmissionKey::mint().expect("the only mint in this test binary");
    let dispatcher = crate::SingleUser::new(
        config.clone(),
        ClientDeps::production(memory.clone()).unwrap(),
    )
    .dispatcher(key)
    .unwrap();
    let ctx = || {
        TrustedRoot::single_user_stdio()
            .admit(
                AdmissionInput {
                    mcp_request_id: "1".into(),
                    protocol_version: ProtocolVersion::new("2025-06-18"),
                    client_capabilities: Value::Null,
                    client_name: "test".into(),
                    instance: None,
                    instance_miss: None,
                    cancel: tokio_util::sync::CancellationToken::new(),
                    deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
                    progress: Arc::new(NoProgress),
                },
                None,
            )
            .unwrap()
    };
    let call = |tool: &'static str, args: Value| {
        let dispatcher = dispatcher.clone();
        let ctx = ctx();
        async move {
            match dispatcher.call_tool(&ctx, tool, args).await {
                Ok(DispatchOutcome::Rendered(r)) => r,
                other => panic!("{tool}: {other:?}"),
            }
        }
    };

    let r = call(
        "SN-Register-Instance",
        json!({"name": "dev", "url": DEV_ORIGIN, "authType": "basic", "username": "developer"}),
    )
    .await;
    assert_ne!(r.is_error, Some(true), "{}", r.text);
    let body: Value = serde_json::from_str(&r.text).unwrap();
    assert_eq!(body["success"], json!(true), "{}", r.text);
    assert_eq!(body["restartRequired"], json!(false));
    assert!(!r.text.contains("fixture-secret-value"));

    let written = std::fs::read_to_string(&file).unwrap();
    assert!(!written.contains("fixture-secret-value"));
    let document: Value = serde_json::from_str(&written).unwrap();
    let instances = document["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 2, "{written}");
    let dev = instances.iter().find(|i| i["name"] == "dev").unwrap();
    assert_eq!(dev["url"], json!(DEV_ORIGIN));
    assert_eq!(dev["authType"], json!("basic"));
    assert_eq!(dev["username"], json!("developer"));
    assert_eq!(dev["credentialRef"], json!(DEV_REF));
    for key in ["password", "clientSecret"] {
        assert!(dev.get(key).is_none(), "{key} in {written}");
    }
    // The saved secret and its binding are untouched.
    assert_eq!(
        InstanceCredentialStore::new(memory.clone())
            .bound_origin(DEV_REF)
            .await
            .unwrap()
            .as_deref(),
        Some(DEV_ORIGIN)
    );

    let r = call("SN-Set-Instance", json!({"instance_name": "dev"})).await;
    assert_ne!(r.is_error, Some(true), "{}", r.text);
    assert!(
        r.text.contains("Switched to ServiceNow instance: dev"),
        "{}",
        r.text
    );
}
