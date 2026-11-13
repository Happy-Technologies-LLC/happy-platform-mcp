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
    s.set_secret(&r, "fixture-secret", ORIGIN).await.unwrap();
    assert_eq!(
        mem.value(SERVICE_NAME, &r).as_deref(),
        Some("fixture-secret")
    );
    assert_eq!(s.get_secret(&r).await.unwrap().expose(), "fixture-secret");
    assert!(s.set_secret(&r, "", ORIGIN).await.is_err());
}

const ORIGIN: &str = "https://dev.example.com";

#[tokio::test]
async fn secrets_are_bound_at_write_time_and_only_read_for_their_origin() {
    let (mem, s) = store();
    let r = "keychain:instance/dev/password";
    s.set_secret(r, "fixture-secret", ORIGIN).await.unwrap();
    assert_eq!(s.bound_origin(r).await.unwrap().as_deref(), Some(ORIGIN));
    assert_eq!(
        s.get_secret_for_origin(r, ORIGIN).await.unwrap().expose(),
        "fixture-secret"
    );
    let moved = s
        .get_secret_for_origin(r, "https://attacker.example")
        .await
        .unwrap_err();
    assert_eq!(moved.code, "CREDENTIAL_ORIGIN_MISMATCH");
    assert!(!moved.message.contains("fixture-secret"));

    // A 6.x secret (no binding) is never returned; binding it makes it usable.
    mem.insert(SERVICE_NAME, "keychain:instance/old/password", "legacy");
    let unbound = s
        .get_secret_for_origin("keychain:instance/old/password", ORIGIN)
        .await
        .unwrap_err();
    assert_eq!(unbound.code, "CREDENTIAL_ORIGIN_UNBOUND");
    assert!(
        unbound
            .message
            .contains("happy-platform-mcp instance credential bind old")
    );
    // M-D: a wrong re-entered secret binds nothing.
    let wrong = s
        .bind_origin("keychain:instance/old/password", ORIGIN, "guess")
        .await
        .unwrap_err();
    assert_eq!(wrong.code, "CREDENTIAL_SECRET_MISMATCH");
    assert!(!wrong.message.contains("legacy"));
    assert!(
        mem.value(
            SERVICE_NAME,
            &origin_binding_account("keychain:instance/old/password")
        )
        .is_none()
    );
    // L-8: a stale binding item is deleted before the new one is written.
    mem.insert(
        SERVICE_NAME,
        &origin_binding_account("keychain:instance/old/password"),
        "stale",
    );
    s.bind_origin("keychain:instance/old/password", ORIGIN, "legacy")
        .await
        .unwrap();
    assert!(
        mem.calls()
            .iter()
            .any(|(op, account)| *op == MemoryOp::Delete
                && *account == origin_binding_account("keychain:instance/old/password"))
    );
    assert!(
        s.get_secret_for_origin("keychain:instance/old/password", ORIGIN)
            .await
            .is_ok()
    );

    // Rollback restores exactly the prior binding, including none.
    s.restore_secret(r, "prior", None).await.unwrap();
    assert_eq!(s.bound_origin(r).await.unwrap(), None);
    s.restore_secret(r, "prior", Some(ORIGIN)).await.unwrap();
    assert_eq!(s.bound_origin(r).await.unwrap().as_deref(), Some(ORIGIN));
    // ...with a MAC that verifies for the restored value (M-B).
    let item = mem.value(SERVICE_NAME, &origin_binding_account(r)).unwrap();
    assert_eq!(
        verified_binding_origin(r, "prior", &item).as_deref(),
        Some(ORIGIN)
    );
    // The secret item itself stays the plain value 6.x reads.
    assert_eq!(mem.value(SERVICE_NAME, r).as_deref(), Some("prior"));
    // A value replaced outside the store (e.g. by 6.x) invalidates the MAC.
    mem.insert(SERVICE_NAME, r, "changed-elsewhere");
    assert_eq!(s.bound_origin(r).await.unwrap(), None);
    s.restore_secret(r, "prior", Some(ORIGIN)).await.unwrap();

    // A failed write leaves the secret unbound (never sent), not bound to
    // the old origin.
    mem.fail_next(MemoryOp::Set);
    assert!(
        s.set_secret(r, "new", "https://other.example")
            .await
            .is_err()
    );
    assert_eq!(s.bound_origin(r).await.unwrap(), None);
}

/// Review L-9 R2: the binding MAC covers the reference. A MAC computed
/// without it, or a valid binding copied to another reference holding the
/// same secret, does not verify.
#[tokio::test]
async fn binding_mac_requires_the_reference() {
    let (mem, s) = store();
    let a = "keychain:instance/a/password";
    let b = "keychain:instance/b/password";
    s.set_secret(a, "same-secret", ORIGIN).await.unwrap();
    mem.insert(SERVICE_NAME, b, "same-secret");

    // HMAC over label and origin only (no reference).
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"same-secret");
    let mut msg = b"happy-platform-mcp/origin-binding/v1".to_vec();
    msg.push(0);
    msg.push(0);
    msg.extend_from_slice(ORIGIN.as_bytes());
    let mac: String = ring::hmac::sign(&key, &msg)
        .as_ref()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    let no_ref = serde_json::json!({"origin": ORIGIN, "mac": mac}).to_string();
    assert_eq!(verified_binding_origin(b, "same-secret", &no_ref), None);
    mem.insert(SERVICE_NAME, &origin_binding_account(b), &no_ref);
    assert_eq!(s.bound_origin(b).await.unwrap(), None);

    // a's valid binding copied onto b.
    let copied = mem.value(SERVICE_NAME, &origin_binding_account(a)).unwrap();
    assert_eq!(
        verified_binding_origin(a, "same-secret", &copied).as_deref(),
        Some(ORIGIN)
    );
    mem.insert(SERVICE_NAME, &origin_binding_account(b), &copied);
    assert_eq!(s.bound_origin(b).await.unwrap(), None);
    assert_eq!(
        s.get_secret_for_origin(b, ORIGIN).await.unwrap_err().code,
        "CREDENTIAL_ORIGIN_UNBOUND"
    );
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
    let err = s.set_secret(r, "fixture-secret", ORIGIN).await.unwrap_err();
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
