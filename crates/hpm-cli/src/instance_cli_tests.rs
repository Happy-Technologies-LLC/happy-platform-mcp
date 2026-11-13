//! Port of `tests/instance-cli.test.js` and `tests/cli.test.js`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use hpm_core::keychain::{MemoryOp, MemorySecretStore, SecretStoreError};
use hpm_core::token_store::InMemoryTokenStore;
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
    ) -> BoxFut<'a, Result<Option<String>, CodedError>> {
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
            credentials: InstanceCredentialStore::new(self.secrets.clone()),
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
    let (p, calls) = prompts(json!({"secret": "fixture-secret-value"}));
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
    assert!(err.contains("path=/tmp/registry.json"));
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
    assert!(err.contains("path=/tmp/registry.json"));
}

#[tokio::test]
async fn reports_credential_rollback_failure_without_replacing_the_registration_cause() {
    let registry = FakeRegistry::with(vec![]);
    *registry.fail_register.lock().unwrap() = Some(registry_error());
    let h = Harness::new(registry, prompts(add_answers("new-instance")).0);
    *h.secrets.fail_delete_at.lock().unwrap() = Some(1);
    let (code, _, err) = h.run(&["instance", "add"]).await;
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("REGISTRY_WRITE_FAILED"), "{err}");
    assert!(err.contains("path=/tmp/registry.json"));
    assert!(err.contains("rollback failed"));
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
    assert!(err.contains("path=/tmp/registry.json"));
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
        prompts(json!({"secret": "new-password"})).0,
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
    let (mut p, _) = prompts(json!({}));
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
    assert_eq!(secrets.count(), 3);
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
            store.get_refresh_token(&other).await.unwrap().as_deref(),
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
}

#[test]
fn dispatch_delegates_server_startup_arguments_to_stdio() {
    assert_eq!(route_of(&[]).0, Dispatch::Serve);
    assert_eq!(route_of(&["--docs-only"]).0, Dispatch::Serve);
    assert_eq!(route_of(&["--help"]).0, Dispatch::Exit(0));
}
