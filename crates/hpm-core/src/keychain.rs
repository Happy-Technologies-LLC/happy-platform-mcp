//! OS credential store (lane A implements; lane B's single-user `CredentialProvider`
//! impls consume). Ports the keychain use in `src/instance-credential-store.js` and
//! `src/token-store.js` (`@napi-rs/keyring` `Entry(service, account)`).
//!
//! Byte compatibility: the S3 keychain vectors (`spikes/s3-packaging/tests/
//! keychain_vectors.rs`) prove that items written by 6.x under [`SERVICE_NAME`] are
//! read back by keyring-core 1.x with identical service and account bytes. An
//! implementation must pass those vectors; it must not rewrite, prefix or normalize
//! either string.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError};

use hpm_types::BoxFut;
use hpm_types::credential::Secret;

use crate::config::CodedError;

/// Keychain service of every 6.x item (credential refs and OAuth refresh tokens).
pub const SERVICE_NAME: &str = "happy-platform-mcp";

#[derive(thiserror::Error)]
#[non_exhaustive]
pub enum SecretStoreError {
    /// The OS store is missing, locked or failed its health probe
    /// (JS `KeychainUnavailableError`). Fails closed; never falls back to a file.
    /// The source never carries stored bytes (see `map_keyring_error`).
    #[error("keychain unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The store accepted the call but did not perform it (JS
    /// `KeychainOperationError`, e.g. a delete that reported `false`).
    #[error("keychain operation failed")]
    Operation,
    /// `service`/`account` rejected by the store or the caller's grammar. Never
    /// echoes the account.
    #[error("invalid keychain account")]
    InvalidAccount,
}

/// Hand-written so a `{:?}` (or `unwrap`) prints only the source's `Display`,
/// never a nested `Debug` that could carry stored bytes.
impl fmt::Debug for SecretStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(source) => write!(f, "Unavailable({source})"),
            Self::Operation => f.write_str("Operation"),
            Self::InvalidAccount => f.write_str("InvalidAccount"),
        }
    }
}

/// Runs blocking store/file I/O on tokio's blocking pool when a runtime is
/// present (Keychain may wait on a user prompt, Secret Service on D-Bus), so an
/// executor worker never stalls. Without a runtime it runs inline.
pub(crate) async fn blocking<T, E>(
    op: impl FnOnce() -> Result<T, E> + Send + 'static,
    join_failed: impl FnOnce() -> E,
) -> Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return op();
    }
    match tokio::task::spawn_blocking(op).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(join_failed()),
    }
}

/// Generic password items addressed by `(service, account)`. Object-safe; used as
/// `Arc<dyn SecretStore>`. Values are never logged; errors never echo them.
pub trait SecretStore: Send + Sync {
    /// `Ok(None)` when no item exists. An empty or whitespace-only stored value is
    /// reported as `None` (JS `getSecret`/`hasSecret` parity).
    fn get<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<Secret>, SecretStoreError>>;

    /// Creates or replaces the item. Callers reject empty values before calling.
    fn set<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
        value: Secret,
    ) -> BoxFut<'a, Result<(), SecretStoreError>>;

    /// `Ok(true)` when an item was deleted, `Ok(false)` when none existed.
    fn delete<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<bool, SecretStoreError>>;
}

fn present(value: String) -> Option<Secret> {
    if value.trim().is_empty() {
        None
    } else {
        Some(Secret::new(value))
    }
}

// ---------------------------------------------------------------------------
// OS store (keyring-core)

/// The platform credential store: macOS Keychain (legacy keychain, as
/// `@napi-rs/keyring`), Windows Credential Manager, Secret Service elsewhere.
pub struct OsSecretStore(());

static DEFAULT_STORE: LazyLock<Result<(), String>> = LazyLock::new(init_default_store);

fn init_default_store() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    let store = windows_native_keyring_store::Store::new().map_err(|e| e.to_string())?;
    #[cfg(all(unix, not(target_os = "macos")))]
    let store = dbus_secret_service_keyring_store::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

impl OsSecretStore {
    /// Installs the platform store as keyring-core's default (once per process).
    pub fn new() -> Result<Self, SecretStoreError> {
        match &*DEFAULT_STORE {
            Ok(()) => Ok(Self(())),
            Err(e) => Err(SecretStoreError::Unavailable(e.clone().into())),
        }
    }

    fn entry(service: &str, account: &str) -> Result<keyring_core::Entry, SecretStoreError> {
        keyring_core::Entry::new(service, account).map_err(map_keyring_error)
    }

    async fn run<T: Send + 'static>(
        service: &str,
        account: &str,
        op: impl FnOnce(keyring_core::Entry) -> Result<T, SecretStoreError> + Send + 'static,
    ) -> Result<T, SecretStoreError> {
        let (service, account) = (service.to_owned(), account.to_owned());
        blocking(
            move || op(Self::entry(&service, &account)?),
            || SecretStoreError::Unavailable("keychain task failed".into()),
        )
        .await
    }
}

/// `BadEncoding`/`BadDataFormat` carry the stored item's raw bytes: they are
/// replaced by an opaque message so no error path can ever print them.
pub(crate) fn map_keyring_error(e: keyring_core::Error) -> SecretStoreError {
    use keyring_core::Error as E;
    match e {
        E::Invalid(..) | E::TooLong(..) => SecretStoreError::InvalidAccount,
        E::BadEncoding(_) | E::BadDataFormat(..) => {
            SecretStoreError::Unavailable("stored keychain item is not readable".into())
        }
        other => SecretStoreError::Unavailable(Box::new(other)),
    }
}

impl SecretStore for OsSecretStore {
    fn get<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<Secret>, SecretStoreError>> {
        Box::pin(Self::run(service, account, |entry| {
            match entry.get_password() {
                Ok(v) => Ok(present(v)),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(e) => Err(map_keyring_error(e)),
            }
        }))
    }

    fn set<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
        value: Secret,
    ) -> BoxFut<'a, Result<(), SecretStoreError>> {
        Box::pin(Self::run(service, account, move |entry| {
            entry
                .set_password(value.expose())
                .map_err(map_keyring_error)
        }))
    }

    fn delete<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<bool, SecretStoreError>> {
        Box::pin(Self::run(service, account, |entry| {
            match entry.delete_credential() {
                Ok(()) => Ok(true),
                Err(keyring_core::Error::NoEntry) => Ok(false),
                Err(e) => Err(map_keyring_error(e)),
            }
        }))
    }
}

// ---------------------------------------------------------------------------
// Lazy store (CLI)

/// Constructor of the inner store of a [`LazySecretStore`].
pub type StoreFactory = Box<dyn FnOnce() -> Result<Arc<dyn SecretStore>, SecretStoreError> + Send>;

/// Builds the inner store on first get/set/delete (6.2.0 imports the keyring
/// module only when a credential operation runs). `instance --help`, `list`
/// and usage errors therefore work without a keychain (headless Linux, Docker);
/// an init failure surfaces from the operation that needed the store.
pub struct LazySecretStore {
    factory: Mutex<Option<StoreFactory>>,
    store: OnceLock<Result<Arc<dyn SecretStore>, String>>,
}

impl LazySecretStore {
    pub fn new(factory: StoreFactory) -> Self {
        Self {
            factory: Mutex::new(Some(factory)),
            store: OnceLock::new(),
        }
    }

    /// The OS store, opened on first use.
    pub fn os() -> Self {
        Self::new(Box::new(|| {
            OsSecretStore::new().map(|s| Arc::new(s) as Arc<dyn SecretStore>)
        }))
    }

    fn inner(&self) -> Result<&Arc<dyn SecretStore>, SecretStoreError> {
        self.store
            .get_or_init(|| {
                let factory = self
                    .factory
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                match factory {
                    Some(f) => f().map_err(|e| e.to_string()),
                    None => Err("keychain unavailable".to_owned()),
                }
            })
            .as_ref()
            .map_err(|e| SecretStoreError::Unavailable(e.clone().into()))
    }
}

impl SecretStore for LazySecretStore {
    fn get<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<Secret>, SecretStoreError>> {
        match self.inner() {
            Ok(s) => s.get(service, account),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn set<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
        value: Secret,
    ) -> BoxFut<'a, Result<(), SecretStoreError>> {
        match self.inner() {
            Ok(s) => s.set(service, account, value),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn delete<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<bool, SecretStoreError>> {
        match self.inner() {
            Ok(s) => s.delete(service, account),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
}

// ---------------------------------------------------------------------------
// In-memory store (tests)

/// Operation selector for [`MemorySecretStore::fail_next`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryOp {
    Get,
    Set,
    Delete,
}

/// Process-local store for tests. Supports one-shot fault injection.
#[derive(Default)]
pub struct MemorySecretStore {
    items: Mutex<HashMap<(String, String), String>>,
    failures: Mutex<VecDeque<MemoryOp>>,
    calls: Mutex<Vec<(MemoryOp, String)>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&self, service: &str, account: &str, value: &str) {
        self.lock_items()
            .insert((service.into(), account.into()), value.into());
    }
    pub fn value(&self, service: &str, account: &str) -> Option<String> {
        self.lock_items()
            .get(&(service.into(), account.into()))
            .cloned()
    }
    /// Sorted accounts stored under `service`.
    pub fn accounts(&self, service: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .lock_items()
            .keys()
            .filter(|(s, _)| s == service)
            .map(|(_, a)| a.clone())
            .collect();
        out.sort();
        out
    }
    /// The next call of `op` fails with [`SecretStoreError::Unavailable`].
    pub fn fail_next(&self, op: MemoryOp) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(op);
    }
    /// Every call so far, in order, as `(op, account)`.
    pub fn calls(&self) -> Vec<(MemoryOp, String)> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    fn lock_items(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), String>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn check(&self, op: MemoryOp, account: &str) -> Result<(), SecretStoreError> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((op, account.to_owned()));
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(pos) = failures.iter().position(|f| *f == op) {
            failures.remove(pos);
            return Err(SecretStoreError::Unavailable("injected failure".into()));
        }
        Ok(())
    }
}

impl SecretStore for MemorySecretStore {
    fn get<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<Secret>, SecretStoreError>> {
        Box::pin(async move {
            self.check(MemoryOp::Get, account)?;
            Ok(self.value(service, account).and_then(present))
        })
    }
    fn set<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
        value: Secret,
    ) -> BoxFut<'a, Result<(), SecretStoreError>> {
        Box::pin(async move {
            self.check(MemoryOp::Set, account)?;
            self.insert(service, account, value.expose());
            Ok(())
        })
    }
    fn delete<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<bool, SecretStoreError>> {
        Box::pin(async move {
            self.check(MemoryOp::Delete, account)?;
            Ok(self
                .lock_items()
                .remove(&(service.into(), account.into()))
                .is_some())
        })
    }
}

// ---------------------------------------------------------------------------
// instance-credential-store.js

pub const CREDENTIAL_PREFIX: &str = "keychain:instance/";

/// Keychain account holding the origin a saved secret is bound to (owner
/// rule: a secret is only sent to that origin). Same service as the secret.
pub fn origin_binding_account(reference: &str) -> String {
    format!(
        "keychain:instance-origin/{}",
        reference
            .strip_prefix(CREDENTIAL_PREFIX)
            .unwrap_or(reference)
    )
}

/// Domain label of the origin-binding MAC (review M-B).
const BINDING_MAC_LABEL: &[u8] = b"happy-platform-mcp/origin-binding/v1";

fn binding_mac_input(reference: &str, origin: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(BINDING_MAC_LABEL.len() + reference.len() + origin.len() + 2);
    msg.extend_from_slice(BINDING_MAC_LABEL);
    msg.push(0);
    msg.extend_from_slice(reference.as_bytes());
    msg.push(0);
    msg.extend_from_slice(origin.as_bytes());
    msg
}

/// The stored binding item: the origin in clear (for error text) plus
/// HMAC-SHA256 keyed by the secret over label, reference and origin, so only
/// a holder of the secret can create a binding that verifies.
pub fn origin_binding_value(reference: &str, secret: &str, origin: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = ring::hmac::sign(&key, &binding_mac_input(reference, origin));
    let mac: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    serde_json::json!({"origin": origin, "mac": mac}).to_string()
}

/// The origin of a stored binding item, only when its MAC verifies for
/// `secret` (constant-time). A plain, forged, copied or stale item is `None`.
pub fn verified_binding_origin(reference: &str, secret: &str, stored: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stored).ok()?;
    let origin = v.get("origin")?.as_str()?;
    let hex = v.get("mac")?.as_str()?;
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mac = (0..32)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    ring::hmac::verify(&key, &binding_mac_input(reference, origin), &mac).ok()?;
    Some(origin.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialType {
    Password,
    ClientSecret,
}

impl CredentialType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::ClientSecret => "client-secret",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "password" => Some(Self::Password),
            "client-secret" => Some(Self::ClientSecret),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCredentialRef {
    pub reference: String,
    pub instance_name: String,
    pub kind: CredentialType,
}

/// `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`
pub fn is_valid_instance_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

pub fn invalid_credential_ref() -> CodedError {
    CodedError::new(
        "INVALID_CREDENTIAL_REF",
        "Invalid instance credential reference",
    )
}

pub fn credential_not_found(reference: &str) -> CodedError {
    CodedError::new(
        "CREDENTIAL_NOT_FOUND",
        format!("Credential not found for reference \"{reference}\""),
    )
    .with("ref", reference)
}

pub fn keychain_unavailable() -> CodedError {
    CodedError::new("KEYCHAIN_UNAVAILABLE", "Keychain backend unavailable")
}

pub fn keychain_operation_failed() -> CodedError {
    CodedError::new("KEYCHAIN_OPERATION_FAILED", "Keychain operation failed")
}

/// The web origin (`scheme://host[:port]`) a secret is bound to.
pub fn instance_origin(url: &str) -> Result<String, CodedError> {
    let origin = url::Url::parse(url)
        .map_err(|_| invalid_instance_origin())?
        .origin()
        .ascii_serialization();
    if origin == "null" {
        return Err(invalid_instance_origin());
    }
    Ok(origin)
}

fn invalid_instance_origin() -> CodedError {
    CodedError::new(
        "INVALID_INSTANCE_URL",
        "The instance URL has no web origin to bind the credential to",
    )
}

/// A saved secret bound to another origin than the instance now registered
/// under its name. Only origins are named.
pub fn credential_origin_mismatch(name: &str, bound: &str, current: &str) -> CodedError {
    CodedError::new(
        crate::client::error::CREDENTIAL_ORIGIN_MISMATCH_CODE,
        format!(
            "The saved credential for instance '{name}' was saved for {bound}, but the instance now points to {current}; it will not be sent there. Check the instance URL first: if {current} is not your instance, fix the URL. Only if the instance really moved, re-save the credential for {current} with: happy-platform-mcp instance credential set {name} (it asks you to confirm the new URL)"
        ),
    )
}

/// A saved secret with no origin binding (saved by 6.x, or a failed write).
pub fn credential_origin_unbound(name: &str) -> CodedError {
    CodedError::new(
        crate::client::error::CREDENTIAL_ORIGIN_UNBOUND_CODE,
        format!(
            "The saved credential for instance '{name}' is not bound to an instance URL yet; it will not be sent. Bind it once to the registered URL with: happy-platform-mcp instance credential bind {name}"
        ),
    )
}

fn store_error(e: SecretStoreError) -> CodedError {
    match e {
        SecretStoreError::Unavailable(_) => keychain_unavailable(),
        SecretStoreError::InvalidAccount => invalid_credential_ref(),
        _ => keychain_operation_failed(),
    }
}

pub fn parse_credential_ref(reference: &str) -> Result<ParsedCredentialRef, CodedError> {
    if reference.contains('?') || reference.contains('#') {
        return Err(invalid_credential_ref());
    }
    let rest = reference
        .strip_prefix(CREDENTIAL_PREFIX)
        .ok_or_else(invalid_credential_ref)?;
    let segments: Vec<&str> = rest.split('/').collect();
    let [name, kind] = segments.as_slice() else {
        return Err(invalid_credential_ref());
    };
    if !is_valid_instance_name(name) {
        return Err(invalid_credential_ref());
    }
    let kind = CredentialType::parse(kind).ok_or_else(invalid_credential_ref)?;
    Ok(ParsedCredentialRef {
        reference: reference.to_owned(),
        instance_name: (*name).to_owned(),
        kind,
    })
}

pub fn credential_ref_for(instance_name: &str, kind: CredentialType) -> Result<String, CodedError> {
    if !is_valid_instance_name(instance_name) {
        return Err(invalid_credential_ref());
    }
    Ok(format!(
        "{CREDENTIAL_PREFIX}{instance_name}/{}",
        kind.as_str()
    ))
}

/// Per-instance credentials addressed by canonical references
/// (`keychain:instance/<name>/<password|client-secret>`), stored under
/// [`SERVICE_NAME`] with the reference as the account.
#[derive(Clone)]
pub struct InstanceCredentialStore {
    store: Arc<dyn SecretStore>,
    service: String,
}

impl InstanceCredentialStore {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self::with_service(store, SERVICE_NAME)
    }
    pub fn with_service(store: Arc<dyn SecretStore>, service: &str) -> Self {
        Self {
            store,
            service: service.to_owned(),
        }
    }

    /// A missing item is ambiguous (absent vs. silently failing backend); a
    /// set/get/delete round-trip on a random account tells them apart.
    async fn verify_backend_health(&self) -> Result<(), CodedError> {
        let account = format!("keychain:health-check/{}", uuid::Uuid::now_v7());
        let probe = format!("health-check-{}", uuid::Uuid::now_v7());
        let primary = async {
            self.store
                .set(&self.service, &account, Secret::new(probe.clone()))
                .await
                .map_err(|_| keychain_unavailable())?;
            match self.store.get(&self.service, &account).await {
                Ok(Some(v)) if v.expose() == probe => Ok(()),
                _ => Err(keychain_unavailable()),
            }
        }
        .await;
        let cleanup = matches!(self.store.delete(&self.service, &account).await, Ok(true));
        primary?;
        if !cleanup {
            return Err(keychain_unavailable());
        }
        Ok(())
    }

    pub async fn get_secret(&self, reference: &str) -> Result<Secret, CodedError> {
        let parsed = parse_credential_ref(reference)?;
        match self
            .store
            .get(&self.service, &parsed.reference)
            .await
            .map_err(store_error)?
        {
            Some(v) => Ok(v),
            None => {
                self.verify_backend_health().await?;
                Err(credential_not_found(&parsed.reference))
            }
        }
    }

    /// Saves `value` bound to `origin` (owner rule: a saved secret is only ever
    /// sent to the origin it was stored for). See [`Self::restore_secret`].
    pub async fn set_secret(
        &self,
        reference: &str,
        value: &str,
        origin: &str,
    ) -> Result<(), CodedError> {
        self.restore_secret(reference, value, Some(origin)).await
    }

    /// Writes `value` with exactly `binding` (`None`: unbound, as a rollback of
    /// an unbound 6.x secret). The old binding is cleared first and the new one
    /// written last, so any partial failure leaves the secret unbound, which
    /// is never sent (fail closed).
    pub async fn restore_secret(
        &self,
        reference: &str,
        value: &str,
        binding: Option<&str>,
    ) -> Result<(), CodedError> {
        let parsed = parse_credential_ref(reference)?;
        if value.is_empty() {
            return Err(CodedError::plain(
                "Credential value must be a non-empty string",
            ));
        }
        if binding.is_some_and(|o| o.is_empty() || o == "null") {
            return Err(invalid_instance_origin());
        }
        self.clear_origin_binding(&parsed.reference).await?;
        self.store
            .set(
                &self.service,
                &parsed.reference,
                Secret::new(value.to_owned()),
            )
            .await
            .map_err(store_error)?;
        match binding {
            Some(origin) => self.write_binding(&parsed.reference, value, origin).await,
            None => Ok(()),
        }
    }

    /// The origin `reference`'s secret is bound to (`None`: unbound, which
    /// includes a binding item whose MAC does not verify for the saved secret).
    pub async fn bound_origin(&self, reference: &str) -> Result<Option<String>, CodedError> {
        let parsed = parse_credential_ref(reference)?;
        let get = |account: String| async move {
            self.store
                .get(&self.service, &account)
                .await
                .map_err(store_error)
        };
        let Some(stored) = get(origin_binding_account(&parsed.reference)).await? else {
            return Ok(None);
        };
        let Some(secret) = get(parsed.reference.clone()).await? else {
            return Ok(None);
        };
        Ok(verified_binding_origin(
            &parsed.reference,
            secret.expose(),
            stored.expose(),
        ))
    }

    /// Binds `reference`'s existing secret to `origin`, a normalised web
    /// origin (`instance credential bind`).
    pub async fn bind_origin(&self, reference: &str, origin: &str) -> Result<(), CodedError> {
        let parsed = parse_credential_ref(reference)?;
        if instance_origin(origin).ok().as_deref() != Some(origin) {
            return Err(invalid_instance_origin());
        }
        let secret = self.get_secret(&parsed.reference).await?;
        self.write_binding(&parsed.reference, secret.expose(), origin)
            .await
    }

    async fn write_binding(
        &self,
        reference: &str,
        secret: &str,
        origin: &str,
    ) -> Result<(), CodedError> {
        self.store
            .set(
                &self.service,
                &origin_binding_account(reference),
                Secret::new(origin_binding_value(reference, secret, origin)),
            )
            .await
            .map_err(store_error)
    }

    /// The secret, only when it is bound to `origin`; an unbound or
    /// differently bound secret is refused without being returned. The MAC
    /// is checked against the secret read here, so a value/binding pair from
    /// two different writes never verifies.
    pub async fn get_secret_for_origin(
        &self,
        reference: &str,
        origin: &str,
    ) -> Result<Secret, CodedError> {
        let parsed = parse_credential_ref(reference)?;
        let secret = self.get_secret(&parsed.reference).await?;
        let stored = self
            .store
            .get(&self.service, &origin_binding_account(&parsed.reference))
            .await
            .map_err(store_error)?;
        let name = parsed.instance_name;
        match stored
            .and_then(|s| verified_binding_origin(&parsed.reference, secret.expose(), s.expose()))
        {
            Some(bound) if bound == origin => Ok(secret),
            Some(bound) => Err(credential_origin_mismatch(&name, &bound, origin)),
            None => Err(credential_origin_unbound(&name)),
        }
    }

    async fn clear_origin_binding(&self, reference: &str) -> Result<(), CodedError> {
        self.store
            .delete(&self.service, &origin_binding_account(reference))
            .await
            .map(|_| ())
            .map_err(store_error)
    }

    pub async fn has_secret(&self, reference: &str) -> Result<bool, CodedError> {
        let parsed = parse_credential_ref(reference)?;
        match self
            .store
            .get(&self.service, &parsed.reference)
            .await
            .map_err(store_error)?
        {
            Some(_) => Ok(true),
            None => {
                self.verify_backend_health().await?;
                Ok(false)
            }
        }
    }

    /// `Ok(false)` when no item existed.
    pub async fn delete_secret(&self, reference: &str) -> Result<bool, CodedError> {
        let parsed = parse_credential_ref(reference)?;
        let existing = self
            .store
            .get(&self.service, &parsed.reference)
            .await
            .map_err(store_error)?;
        if existing.is_none() {
            self.verify_backend_health().await?;
            return Ok(false);
        }
        drop(existing);
        if !self
            .store
            .delete(&self.service, &parsed.reference)
            .await
            .map_err(store_error)?
        {
            return Err(keychain_operation_failed());
        }
        self.clear_origin_binding(&parsed.reference).await?;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "keychain_tests.rs"]
mod tests;
