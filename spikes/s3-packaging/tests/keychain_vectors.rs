//! JS <-> Rust keychain parity vectors (service `happy-platform-mcp`).
//!
//! These tests need a real OS credential store, so they are `#[ignore]` and are driven by
//! `scripts/run-keychain-vectors.mjs`, which (1) writes the items from the JS code path
//! (`@napi-rs/keyring` through `InstanceCredentialStore` / `KeychainTokenStore`), (2) runs
//! these tests, (3) deletes everything and re-checks absence. `HPM_S3_VECTORS` points at the
//! JSON file the orchestrator wrote (test-only, non-secret values).
//!
//! Stores (keyring-core 1.x): macOS `apple-native-keyring-store` (legacy keychain),
//! Linux `dbus-secret-service-keyring-store`, Windows `windows-native-keyring-store`.

use hpm_s3::{SERVICE_NAME, identity_v1_account};
use keyring_core::{Entry, Error};
use serde_json::Value;

fn init_store() {
    #[cfg(target_os = "macos")]
    keyring_core::set_default_store(apple_native_keyring_store::keychain::Store::new().unwrap());
    #[cfg(target_os = "linux")]
    keyring_core::set_default_store(dbus_secret_service_keyring_store::Store::new().unwrap());
    #[cfg(target_os = "windows")]
    keyring_core::set_default_store(windows_native_keyring_store::Store::new().unwrap());
}

fn vectors() -> Vec<Value> {
    let path = std::env::var("HPM_S3_VECTORS").expect("HPM_S3_VECTORS (run via scripts/run-keychain-vectors.mjs)");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(doc["service"], SERVICE_NAME, "orchestrator must use the production service name");
    doc["vectors"].as_array().unwrap().clone()
}

fn by_direction<'a>(all: &'a [Value], dir: &str) -> Vec<&'a Value> {
    all.iter().filter(|v| v["direction"] == dir).collect()
}

/// Account for a vector. `identity-v1` vectors ignore the JS-supplied account and recompute
/// it in Rust from the tuple inputs, then assert equality with JS before reading.
fn account_for(v: &Value) -> String {
    let js_account = v["account"].as_str().unwrap().to_owned();
    if let Some(i) = v.get("identityInput") {
        let opt = |k: &str| i.get(k).and_then(Value::as_str);
        let rust_account =
            identity_v1_account(i["url"].as_str().unwrap(), i["clientId"].as_str().unwrap(), opt("authorizeUrl"), opt("tokenUrl"), i["osUser"].as_str().unwrap())
                .unwrap();
        assert_eq!(rust_account, js_account, "Rust identity-v1 account != JS");
    }
    js_account
}

#[test]
#[ignore = "needs OS credential store + JS-written items"]
fn js_written_items_are_readable_from_rust() {
    init_store();
    let all = vectors();
    let items = by_direction(&all, "js_to_rust");
    assert!(!items.is_empty());
    for v in items {
        let account = account_for(v);
        let want = v["value"].as_str().unwrap();
        let entry = Entry::new(SERVICE_NAME, &account).unwrap();
        let got = entry.get_password().unwrap_or_else(|e| panic!("read {account}: {e:?}"));
        assert_eq!(got, want, "value mismatch for {account}");
        assert_eq!(entry.get_secret().unwrap(), want.as_bytes(), "secret bytes mismatch for {account}");
        println!("OK js->rust {account} ({} bytes)", want.len());
    }
}

#[test]
#[ignore = "needs OS credential store"]
fn rust_writes_items_for_js() {
    init_store();
    let all = vectors();
    for v in by_direction(&all, "rust_to_js") {
        let account = account_for(v);
        Entry::new(SERVICE_NAME, &account).unwrap().set_password(v["value"].as_str().unwrap()).unwrap();
        println!("OK rust wrote {account}");
    }
}

#[test]
#[ignore = "needs OS credential store"]
fn all_vector_items_are_absent() {
    init_store();
    let all = vectors();
    for v in &all {
        let account = account_for(v);
        match Entry::new(SERVICE_NAME, &account).unwrap().get_password() {
            Err(Error::NoEntry) => println!("OK absent {account}"),
            other => panic!("expected NoEntry for {account}, got {other:?}"),
        }
    }
}
