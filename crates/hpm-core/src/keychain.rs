//! OS credential store (lane A implements; lane B's single-user `CredentialProvider`
//! impls consume). Ports the keychain use in `src/instance-credential-store.js` and
//! `src/token-store.js` (`@napi-rs/keyring` `Entry(service, account)`).
//!
//! Byte compatibility: the S3 keychain vectors (`spikes/s3-packaging/tests/
//! keychain_vectors.rs`) prove that items written by 6.x under [`SERVICE_NAME`] are
//! read back by keyring-core 1.x with identical service and account bytes. An
//! implementation must pass those vectors; it must not rewrite, prefix or normalize
//! either string.

use hpm_types::BoxFut;
use hpm_types::credential::Secret;

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
