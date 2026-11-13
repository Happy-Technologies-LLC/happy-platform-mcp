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
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use hpm_types::BoxFut;
use hpm_types::credential::Secret;

use crate::config::CodedError;

/// Keychain service of every 6.x item (credential refs and OAuth refresh tokens).
pub const SERVICE_NAME: &str = "happy-platform-mcp";

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretStoreError {
    /// The OS store is missing, locked or failed its health probe
    /// (JS `KeychainUnavailableError`). Fails closed; never falls back to a file.
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
}

fn map_keyring_error(e: keyring_core::Error) -> SecretStoreError {
    use keyring_core::Error as E;
    match e {
        E::Invalid(..) | E::TooLong(..) => SecretStoreError::InvalidAccount,
        other => SecretStoreError::Unavailable(Box::new(other)),
    }
}

impl SecretStore for OsSecretStore {
    fn get<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<Secret>, SecretStoreError>> {
        Box::pin(async move {
            match Self::entry(service, account)?.get_password() {
                Ok(v) => Ok(present(v)),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(e) => Err(map_keyring_error(e)),
            }
        })
    }

    fn set<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
        value: Secret,
    ) -> BoxFut<'a, Result<(), SecretStoreError>> {
        Box::pin(async move {
            Self::entry(service, account)?
                .set_password(value.expose())
                .map_err(map_keyring_error)
        })
    }

    fn delete<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> BoxFut<'a, Result<bool, SecretStoreError>> {
        Box::pin(async move {
            match Self::entry(service, account)?.delete_credential() {
                Ok(()) => Ok(true),
                Err(keyring_core::Error::NoEntry) => Ok(false),
                Err(e) => Err(map_keyring_error(e)),
            }
        })
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

    pub async fn set_secret(&self, reference: &str, value: &str) -> Result<(), CodedError> {
        let parsed = parse_credential_ref(reference)?;
        if value.is_empty() {
            return Err(CodedError::plain(
                "Credential value must be a non-empty string",
            ));
        }
        self.store
            .set(
                &self.service,
                &parsed.reference,
                Secret::new(value.to_owned()),
            )
            .await
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
        Ok(true)
    }
}

#[cfg(test)]
#[path = "keychain_tests.rs"]
mod tests;
