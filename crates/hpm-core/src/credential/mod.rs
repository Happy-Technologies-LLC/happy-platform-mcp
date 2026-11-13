//! Instance credential references and their keychain store (ports
//! `src/instance-credential-store.js`). References look like
//! `keychain:instance/<name>/<password|client-secret>` and are used verbatim as
//! the keychain account under [`SERVICE_NAME`].

use std::sync::Arc;

use base64::Engine;
use hpm_types::credential::Secret;

use crate::keychain::{
    SERVICE_NAME, SecretStore, SecretStoreError, origin_binding_account, origin_binding_value,
    verified_binding_origin,
};

pub mod provider;

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

    fn parse(value: &str) -> Option<Self> {
        match value {
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

#[derive(Debug, thiserror::Error)]
pub enum CredentialStoreError {
    #[error("Invalid instance credential reference")]
    InvalidRef,
    /// The message names the reference (not a secret), as 6.x does.
    #[error("Credential not found for reference \"{0}\"")]
    NotFound(String),
    #[error("Keychain backend unavailable")]
    Unavailable,
    #[error("Credential value must be a non-empty string")]
    EmptyValue,
    #[error("Keychain operation failed")]
    Operation,
    /// The secret is not bound to any origin; it is never sent.
    #[error("{}", crate::keychain::credential_origin_unbound(.0).message)]
    OriginUnbound(String),
    /// The secret is bound to another origin than the instance URL.
    #[error("{}", crate::keychain::credential_origin_mismatch(&.0.name, &.0.bound, &.0.current).message)]
    OriginMismatch(Box<OriginMismatch>),
    /// A backend failure propagated as-is.
    #[error(transparent)]
    Store(SecretStoreError),
}

/// Names and origins only; never the secret.
#[derive(Debug)]
pub struct OriginMismatch {
    pub name: String,
    pub bound: String,
    pub current: String,
}

impl CredentialStoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRef => "INVALID_CREDENTIAL_REF",
            Self::NotFound(_) => "CREDENTIAL_NOT_FOUND",
            Self::Unavailable | Self::Store(SecretStoreError::Unavailable(_)) => {
                "KEYCHAIN_UNAVAILABLE"
            }
            Self::Operation | Self::Store(SecretStoreError::Operation) => {
                "KEYCHAIN_OPERATION_FAILED"
            }
            Self::Store(SecretStoreError::InvalidAccount) => "INVALID_CREDENTIAL_REF",
            Self::EmptyValue => "KEYCHAIN_OPERATION_FAILED",
            Self::OriginUnbound(_) => crate::client::error::CREDENTIAL_ORIGIN_UNBOUND_CODE,
            Self::OriginMismatch(_) => crate::client::error::CREDENTIAL_ORIGIN_MISMATCH_CODE,
        }
    }
}

fn is_valid_instance_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

/// `parseCredentialRef`.
pub fn parse_credential_ref(reference: &str) -> Result<ParsedCredentialRef, CredentialStoreError> {
    if reference.contains('?') || reference.contains('#') {
        return Err(CredentialStoreError::InvalidRef);
    }
    let rest = reference
        .strip_prefix(CREDENTIAL_PREFIX)
        .ok_or(CredentialStoreError::InvalidRef)?;
    let mut segments = rest.split('/');
    let (Some(name), Some(kind), None) = (segments.next(), segments.next(), segments.next()) else {
        return Err(CredentialStoreError::InvalidRef);
    };
    if !is_valid_instance_name(name) {
        return Err(CredentialStoreError::InvalidRef);
    }
    let kind = CredentialType::parse(kind).ok_or(CredentialStoreError::InvalidRef)?;
    Ok(ParsedCredentialRef {
        reference: reference.to_owned(),
        instance_name: name.to_owned(),
        kind,
    })
}

/// `credentialRefFor`.
pub fn credential_ref_for(instance_name: &str, kind: &str) -> Result<String, CredentialStoreError> {
    if !is_valid_instance_name(instance_name) || CredentialType::parse(kind).is_none() {
        return Err(CredentialStoreError::InvalidRef);
    }
    Ok(format!("{CREDENTIAL_PREFIX}{instance_name}/{kind}"))
}

fn random_token() -> String {
    let mut buf = [0u8; 16];
    // OS RNG failure is unrecoverable; the probe must not use predictable names.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// `InstanceCredentialStore` over the OS secret store.
#[derive(Clone)]
pub struct InstanceCredentialStore {
    store: Arc<dyn SecretStore>,
    service: String,
}

impl InstanceCredentialStore {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            store,
            service: SERVICE_NAME.to_owned(),
        }
    }

    /// `_verifyBackendHealth`: write/read/delete a random probe item. Any
    /// failure (including cleanup) reports the backend unavailable.
    async fn verify_backend_health(&self) -> Result<(), CredentialStoreError> {
        let account = format!("keychain:health-check/{}", random_token());
        let probe = format!("health-check-{}", random_token());
        let primary = match self
            .store
            .set(&self.service, &account, Secret::new(probe.clone()))
            .await
        {
            Ok(()) => match self.store.get(&self.service, &account).await {
                Ok(Some(observed)) if observed.expose() == probe => None,
                Ok(_) => Some(CredentialStoreError::Unavailable),
                Err(SecretStoreError::Unavailable(e)) => Some(CredentialStoreError::Store(
                    SecretStoreError::Unavailable(e),
                )),
                Err(_) => Some(CredentialStoreError::Unavailable),
            },
            Err(SecretStoreError::Unavailable(e)) => Some(CredentialStoreError::Store(
                SecretStoreError::Unavailable(e),
            )),
            Err(_) => Some(CredentialStoreError::Unavailable),
        };
        let cleanup_ok = matches!(self.store.delete(&self.service, &account).await, Ok(true));
        match primary {
            Some(error) => Err(error),
            None if !cleanup_ok => Err(CredentialStoreError::Unavailable),
            None => Ok(()),
        }
    }

    pub async fn get_secret(&self, reference: &str) -> Result<Secret, CredentialStoreError> {
        let parsed = parse_credential_ref(reference)?;
        match self
            .store
            .get(&self.service, &parsed.reference)
            .await
            .map_err(CredentialStoreError::Store)?
        {
            Some(value) => Ok(value),
            None => {
                self.verify_backend_health().await?;
                Err(CredentialStoreError::NotFound(parsed.reference))
            }
        }
    }

    /// Saves `value` bound to `origin`. The old binding is cleared first and
    /// the new one written last, so a partial failure leaves the secret
    /// unbound, which is never sent (fail closed).
    pub async fn set_secret(
        &self,
        reference: &str,
        value: Secret,
        origin: &str,
    ) -> Result<(), CredentialStoreError> {
        let parsed = parse_credential_ref(reference)?;
        if value.expose().is_empty() {
            return Err(CredentialStoreError::EmptyValue);
        }
        if origin.is_empty() || origin == "null" {
            return Err(CredentialStoreError::Operation);
        }
        self.clear_origin_binding(&parsed.reference).await?;
        let binding = origin_binding_value(&parsed.reference, value.expose(), origin);
        self.store
            .set(&self.service, &parsed.reference, value)
            .await
            .map_err(CredentialStoreError::Store)?;
        self.store
            .set(
                &self.service,
                &origin_binding_account(&parsed.reference),
                Secret::new(binding),
            )
            .await
            .map_err(CredentialStoreError::Store)
    }

    /// The origin `reference`'s secret is bound to (`None`: unbound, which
    /// includes a binding item whose MAC does not verify for the secret).
    pub async fn bound_origin(
        &self,
        reference: &str,
    ) -> Result<Option<String>, CredentialStoreError> {
        let parsed = parse_credential_ref(reference)?;
        let Some(secret) = self
            .store
            .get(&self.service, &parsed.reference)
            .await
            .map_err(CredentialStoreError::Store)?
        else {
            return Ok(None);
        };
        self.verified_origin(&parsed.reference, &secret).await
    }

    async fn verified_origin(
        &self,
        reference: &str,
        secret: &Secret,
    ) -> Result<Option<String>, CredentialStoreError> {
        Ok(self
            .store
            .get(&self.service, &origin_binding_account(reference))
            .await
            .map_err(CredentialStoreError::Store)?
            .and_then(|s| verified_binding_origin(reference, secret.expose(), s.expose())))
    }

    /// The secret, only when it is bound to `origin`. Never binds: an unbound
    /// secret is refused until the operator binds it with the CLI. The MAC is
    /// checked against the secret read here (a forged, copied or stale
    /// binding counts as unbound).
    pub async fn get_secret_for_origin(
        &self,
        reference: &str,
        origin: &str,
    ) -> Result<Secret, CredentialStoreError> {
        let parsed = parse_credential_ref(reference)?;
        let secret = self.get_secret(&parsed.reference).await?;
        let name = parsed.instance_name;
        match self.verified_origin(&parsed.reference, &secret).await? {
            Some(bound) if bound == origin => Ok(secret),
            Some(bound) => Err(CredentialStoreError::OriginMismatch(Box::new(
                OriginMismatch {
                    name,
                    bound,
                    current: origin.to_owned(),
                },
            ))),
            None => Err(CredentialStoreError::OriginUnbound(name)),
        }
    }

    /// See [`crate::keychain::origin_binding_account`].
    async fn clear_origin_binding(&self, reference: &str) -> Result<(), CredentialStoreError> {
        self.store
            .delete(&self.service, &origin_binding_account(reference))
            .await
            .map(|_| ())
            .map_err(CredentialStoreError::Store)
    }

    pub async fn has_secret(&self, reference: &str) -> Result<bool, CredentialStoreError> {
        let parsed = parse_credential_ref(reference)?;
        match self
            .store
            .get(&self.service, &parsed.reference)
            .await
            .map_err(CredentialStoreError::Store)?
        {
            Some(_) => Ok(true),
            None => {
                self.verify_backend_health().await?;
                Ok(false)
            }
        }
    }

    /// Returns whether an item was deleted.
    pub async fn delete_secret(&self, reference: &str) -> Result<bool, CredentialStoreError> {
        let parsed = parse_credential_ref(reference)?;
        let existing = self
            .store
            .get(&self.service, &parsed.reference)
            .await
            .map_err(CredentialStoreError::Store)?;
        if existing.is_none() {
            self.verify_backend_health().await?;
            return Ok(false);
        }
        drop(existing);
        if self
            .store
            .delete(&self.service, &parsed.reference)
            .await
            .map_err(CredentialStoreError::Store)?
        {
            self.clear_origin_binding(&parsed.reference).await?;
            Ok(true)
        } else {
            Err(CredentialStoreError::Operation)
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use hpm_types::BoxFut;
    use hpm_types::credential::Secret;

    use crate::keychain::{SecretStore, SecretStoreError};

    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    pub enum Fault {
        #[default]
        None,
        Fail,
        Mismatch,
        Missing,
        DeleteFalse,
    }

    /// In-memory SecretStore with per-operation faults on non-target accounts.
    #[derive(Default)]
    pub struct MemStore {
        pub values: Mutex<HashMap<(String, String), String>>,
        pub calls: Mutex<Vec<(&'static str, String)>>,
        pub probe_get: Fault,
        pub probe_set: Fault,
        pub probe_delete: Fault,
        pub fail_all: Option<&'static str>,
    }

    impl MemStore {
        pub fn put(&self, account: &str, value: &str) {
            self.values
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(("happy-platform-mcp".into(), account.into()), value.into());
        }
        pub fn has(&self, account: &str) -> bool {
            self.values
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .any(|(_, a)| a == account)
        }
        pub fn calls(&self, op: &str) -> Vec<String> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|(o, _)| *o == op)
                .map(|(_, a)| a.clone())
                .collect()
        }
        fn record(&self, op: &'static str, account: &str) {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((op, account.into()));
        }
        fn is_probe(account: &str) -> bool {
            account.starts_with("keychain:health-check/")
        }
    }

    fn locked() -> SecretStoreError {
        SecretStoreError::Unavailable("keychain locked".into())
    }

    impl SecretStore for MemStore {
        fn get<'a>(
            &'a self,
            service: &'a str,
            account: &'a str,
        ) -> BoxFut<'a, Result<Option<Secret>, SecretStoreError>> {
            Box::pin(async move {
                self.record("get", account);
                if self.fail_all == Some("get") {
                    return Err(locked());
                }
                if Self::is_probe(account) {
                    match self.probe_get {
                        Fault::Fail => return Err(SecretStoreError::Operation),
                        Fault::Mismatch => {
                            return Ok(Some(Secret::new("different-probe-value".into())));
                        }
                        Fault::Missing => return Ok(None),
                        _ => {}
                    }
                }
                let map = self.values.lock().unwrap_or_else(|e| e.into_inner());
                Ok(map
                    .get(&(service.to_owned(), account.to_owned()))
                    .filter(|v| !v.trim().is_empty())
                    .map(|v| Secret::new(v.clone())))
            })
        }
        fn set<'a>(
            &'a self,
            service: &'a str,
            account: &'a str,
            value: Secret,
        ) -> BoxFut<'a, Result<(), SecretStoreError>> {
            Box::pin(async move {
                self.record("set", account);
                if self.fail_all == Some("set") {
                    return Err(locked());
                }
                self.values
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        (service.to_owned(), account.to_owned()),
                        value.expose().to_owned(),
                    );
                if Self::is_probe(account) && self.probe_set == Fault::Fail {
                    return Err(SecretStoreError::Operation);
                }
                Ok(())
            })
        }
        fn delete<'a>(
            &'a self,
            service: &'a str,
            account: &'a str,
        ) -> BoxFut<'a, Result<bool, SecretStoreError>> {
            Box::pin(async move {
                self.record("delete", account);
                if self.fail_all == Some("delete") {
                    return Err(locked());
                }
                let deleted = self
                    .values
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&(service.to_owned(), account.to_owned()))
                    .is_some();
                if Self::is_probe(account) {
                    match self.probe_delete {
                        Fault::Fail => return Err(SecretStoreError::Operation),
                        Fault::DeleteFalse => return Ok(false),
                        _ => {}
                    }
                }
                Ok(deleted)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Fault, MemStore};
    use super::*;

    const ORIGIN: &str = "https://dev.example.com";

    fn store(mem: MemStore) -> (Arc<MemStore>, InstanceCredentialStore) {
        let mem = Arc::new(mem);
        (mem.clone(), InstanceCredentialStore::new(mem))
    }

    fn reference() -> String {
        credential_ref_for("dev", "password").unwrap_or_default()
    }

    #[test]
    fn deterministic_references() {
        assert_eq!(reference(), "keychain:instance/dev/password");
        assert_eq!(
            credential_ref_for("prod", "client-secret").ok().as_deref(),
            Some("keychain:instance/prod/client-secret")
        );
    }

    #[test]
    fn parses_without_normalizing() {
        let parsed = parse_credential_ref(&reference()).ok();
        assert_eq!(
            parsed,
            Some(ParsedCredentialRef {
                reference: reference(),
                instance_name: "dev".into(),
                kind: CredentialType::Password
            })
        );
        assert_eq!(
            parse_credential_ref("keychain:instance/Dev/password")
                .map(|p| p.instance_name)
                .ok()
                .as_deref(),
            Some("Dev")
        );
        assert!(parse_credential_ref("keychain:instance/dev%2Fother/password").is_err());
    }

    #[test]
    fn rejects_invalid_names_and_types() {
        for (name, kind) in [
            ("", "password"),
            (" dev", "password"),
            ("dev ", "password"),
            ("../dev", "password"),
            ("dev/other", "password"),
            ("dev?x", "password"),
            ("dev#x", "password"),
            ("dev", ""),
            ("dev", "unknown"),
        ] {
            assert!(credential_ref_for(name, kind).is_err(), "{name:?} {kind:?}");
        }
    }

    #[tokio::test]
    async fn stores_and_retrieves() {
        let (mem, s) = store(MemStore::default());
        assert!(
            s.set_secret(&reference(), Secret::new("fixture-secret".into()), ORIGIN)
                .await
                .is_ok()
        );
        assert_eq!(
            s.get_secret(&reference())
                .await
                .ok()
                .map(|v| v.expose().to_owned())
                .as_deref(),
            Some("fixture-secret")
        );
        assert!(mem.has(&reference()));
    }

    #[tokio::test]
    async fn empty_value_rejected_without_echo() {
        let (_, s) = store(MemStore::default());
        let err = s
            .set_secret(&reference(), Secret::new(String::new()), ORIGIN)
            .await
            .err();
        assert!(matches!(err, Some(CredentialStoreError::EmptyValue)));
    }

    #[tokio::test]
    async fn missing_is_not_found() {
        let (_, s) = store(MemStore::default());
        let err = s.get_secret(&reference()).await.err();
        assert_eq!(
            err.as_ref().map(CredentialStoreError::code),
            Some("CREDENTIAL_NOT_FOUND")
        );
        assert!(
            err.map(|e| e.to_string())
                .unwrap_or_default()
                .contains(&reference())
        );
    }

    #[tokio::test]
    async fn rejects_malformed_references() {
        let (_, s) = store(MemStore::default());
        for r in [
            "keychain:instance/dev",
            "keychain:instance//password",
            "keychain:instance/dev/",
            "keychain:instance/dev/unknown",
            "keychain:instance/../password",
            "keychain:instance/dev/../password",
            "keychain:instance/dev/password?query",
            "keychain:instance/dev/password#fragment",
            "keychain:instance/dev/password/extra",
            "other:instance/dev/password",
        ] {
            assert!(s.has_secret(r).await.is_err(), "{r}");
            assert!(s.get_secret(r).await.is_err());
            assert!(
                s.set_secret(r, Secret::new("fixture-secret".into()), ORIGIN)
                    .await
                    .is_err()
            );
            assert!(s.delete_secret(r).await.is_err());
        }
    }

    #[tokio::test]
    async fn has_secret_false_for_missing_or_blank() {
        let (mem, s) = store(MemStore::default());
        assert_eq!(s.has_secret(&reference()).await.ok(), Some(false));
        for blank in ["", "   ", "\t\n"] {
            mem.put(&reference(), blank);
            assert_eq!(s.has_secret(&reference()).await.ok(), Some(false));
            assert_eq!(
                s.get_secret(&reference()).await.err().map(|e| e.code()),
                Some("CREDENTIAL_NOT_FOUND")
            );
        }
        mem.put(&reference(), "fixture-secret");
        assert_eq!(s.has_secret(&reference()).await.ok(), Some(true));
    }

    #[tokio::test]
    async fn probe_failures_clean_up_and_report_unavailable() {
        for (get, set, delete) in [
            (Fault::Missing, Fault::None, Fault::None),
            (Fault::Mismatch, Fault::None, Fault::None),
            (Fault::Fail, Fault::None, Fault::None),
            (Fault::None, Fault::None, Fault::DeleteFalse),
            (Fault::None, Fault::None, Fault::Fail),
            (Fault::Fail, Fault::None, Fault::Fail),
            (Fault::None, Fault::Fail, Fault::None),
        ] {
            let (mem, s) = store(MemStore {
                probe_get: get,
                probe_set: set,
                probe_delete: delete,
                ..MemStore::default()
            });
            let err = s.has_secret(&reference()).await.err();
            assert_eq!(err.map(|e| e.code()), Some("KEYCHAIN_UNAVAILABLE"));
            let probe = mem.calls("set").first().cloned().unwrap_or_default();
            assert!(mem.calls("delete").contains(&probe));
            assert!(!mem.has(&probe));
        }
    }

    #[tokio::test]
    async fn deletes_and_reports_status() {
        let (_, s) = store(MemStore::default());
        assert!(
            s.set_secret(&reference(), Secret::new("fixture-secret".into()), ORIGIN)
                .await
                .is_ok()
        );
        assert_eq!(s.delete_secret(&reference()).await.ok(), Some(true));
        assert_eq!(s.has_secret(&reference()).await.ok(), Some(false));
        assert_eq!(s.delete_secret(&reference()).await.ok(), Some(false));
    }

    #[tokio::test]
    async fn propagates_backend_failures() {
        for op in ["get", "set", "delete"] {
            let (mem, s) = store(MemStore {
                fail_all: Some(op),
                ..MemStore::default()
            });
            mem.put(&reference(), "fixture-secret");
            let err = match op {
                "get" => s.get_secret(&reference()).await.err(),
                "set" => s
                    .set_secret(&reference(), Secret::new("x".into()), ORIGIN)
                    .await
                    .err(),
                _ => s.delete_secret(&reference()).await.err(),
            };
            assert!(
                matches!(
                    err,
                    Some(CredentialStoreError::Store(SecretStoreError::Unavailable(
                        _
                    )))
                ),
                "{op}"
            );
        }
    }
}
