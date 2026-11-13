//! Port of `tests/instance-credential-store.test.js` over [`MemorySecretStore`]
//! (native-entry construction cases are covered by the store abstraction).

use super::*;

fn store() -> (Arc<MemorySecretStore>, InstanceCredentialStore) {
    let mem = Arc::new(MemorySecretStore::new());
    (mem.clone(), InstanceCredentialStore::new(mem))
}

#[test]
fn deterministic_references() {
    assert_eq!(
        credential_ref_for("dev", CredentialType::Password).unwrap(),
        "keychain:instance/dev/password"
    );
    assert_eq!(
        credential_ref_for("dev", CredentialType::ClientSecret).unwrap(),
        "keychain:instance/dev/client-secret"
    );
    let parsed = parse_credential_ref("keychain:instance/dev/client-secret").unwrap();
    assert_eq!(parsed.instance_name, "dev");
    assert_eq!(parsed.kind, CredentialType::ClientSecret);
    for bad in [
        "keychain:instance/dev%2Fother/password",
        "keychain:instance/dev/password?x",
        "keychain:instance/dev/password#x",
        "keychain:instance/dev/other",
        "keychain:instance/dev/a/password",
        "plaintext-password",
        "keychain:instance/-dev/password",
    ] {
        assert_eq!(
            parse_credential_ref(bad).unwrap_err().code,
            "INVALID_CREDENTIAL_REF",
            "{bad}"
        );
    }
    for name in ["", "-dev", "dev/other", "a b", &"a".repeat(65)] {
        assert!(credential_ref_for(name, CredentialType::Password).is_err());
    }
}

#[tokio::test]
async fn stores_retrieves_and_rejects_empty() {
    let (mem, s) = store();
    let r = credential_ref_for("dev", CredentialType::Password).unwrap();
    s.set_secret(&r, "fixture-secret").await.unwrap();
    assert_eq!(
        mem.value(SERVICE_NAME, &r).as_deref(),
        Some("fixture-secret")
    );
    assert_eq!(s.get_secret(&r).await.unwrap().expose(), "fixture-secret");
    assert!(s.set_secret(&r, "").await.is_err());
}

#[tokio::test]
async fn missing_vs_failure() {
    let (mem, s) = store();
    let r = "keychain:instance/dev/password";
    let err = s.get_secret(r).await.unwrap_err();
    assert_eq!(err.code, "CREDENTIAL_NOT_FOUND");
    assert_eq!(err.detail_str("ref"), Some(r));
    assert!(!s.has_secret(r).await.unwrap());
    mem.insert(SERVICE_NAME, r, "   ");
    assert!(!s.has_secret(r).await.unwrap());
    mem.fail_next(MemoryOp::Get);
    assert_eq!(
        s.get_secret(r).await.unwrap_err().code,
        "KEYCHAIN_UNAVAILABLE"
    );
    mem.fail_next(MemoryOp::Set);
    let err = s.set_secret(r, "fixture-secret").await.unwrap_err();
    assert!(!format!("{err:?}").contains("fixture-secret"));
}

#[tokio::test]
async fn health_probe_failures_are_unavailable() {
    let (mem, s) = store();
    let r = "keychain:instance/dev/password";
    mem.fail_next(MemoryOp::Set);
    assert_eq!(
        s.get_secret(r).await.unwrap_err().code,
        "KEYCHAIN_UNAVAILABLE"
    );
    mem.fail_next(MemoryOp::Delete);
    assert_eq!(
        s.has_secret(r).await.unwrap_err().code,
        "KEYCHAIN_UNAVAILABLE"
    );
}

#[tokio::test]
async fn delete_returns_status() {
    let (mem, s) = store();
    let r = "keychain:instance/dev/password";
    mem.insert(SERVICE_NAME, r, "fixture-secret");
    assert!(s.delete_secret(r).await.unwrap());
    assert!(!s.delete_secret(r).await.unwrap());
    assert!(mem.accounts(SERVICE_NAME).is_empty());
}

/// Review L2: keyring errors that carry the stored bytes are mapped to an opaque
/// variant; neither `Debug` nor `Display` can print a planted secret.
#[test]
fn store_errors_never_format_stored_bytes() {
    let planted = b"planted-secret-value".to_vec();
    let numeric = format!("{:?}", &planted[..4]);
    for e in [
        keyring_core::Error::BadEncoding(planted.clone()),
        keyring_core::Error::BadDataFormat(planted.clone(), "nested".into()),
    ] {
        let mapped = map_keyring_error(e);
        let rendered = [
            format!("{mapped:?}"),
            format!("{mapped:#?}"),
            mapped.to_string(),
            format!("{:?}", Err::<(), _>(mapped)),
        ];
        for text in rendered {
            assert!(!text.contains("planted"), "{text}");
            assert!(!text.contains(numeric.trim_matches(['[', ']'])), "{text}");
        }
    }
}
