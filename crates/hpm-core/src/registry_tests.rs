//! Port of `tests/instance-registry.test.js` (fs-spy fault cases are not
//! portable; precommit/compensation covered via the Rust callbacks).

use super::*;
use crate::keychain::{CredentialType, credential_ref_for};
use serde_json::json;

struct Tmp {
    _dir: tempfile::TempDir,
    file: PathBuf,
    legacy: PathBuf,
    user: PathBuf,
}

fn tmp() -> Tmp {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    Tmp {
        file: root.join("instances.json"),
        legacy: root.join("legacy.json"),
        user: root.join("user/instances.json"),
        _dir: dir,
    }
}

fn write_json(file: &Path, doc: &Value) {
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, format!("{doc}\n")).unwrap();
}

fn public(name: &str, extra: Value) -> Value {
    let mut o = json!({
        "name": name,
        "url": format!("https://{name}.service-now.com"),
        "authType": "oauth",
        "grantType": "authorization_code",
        "clientId": format!("{name}-public-client"),
    });
    for (k, v) in extra.as_object().unwrap() {
        o[k] = v.clone();
    }
    o
}

fn reg(t: &Tmp) -> InstanceRegistry {
    InstanceRegistry::at_paths(&t.file, &t.file, Env::from_pairs(&[]))
}

fn register(r: &InstanceRegistry, v: Value) -> Result<Object, CodedError> {
    r.register(&v, RegisterOptions::default())
}

fn code(r: Result<impl std::fmt::Debug, CodedError>) -> String {
    r.unwrap_err().code
}

#[test]
fn loads_and_preserves_document_properties() {
    let t = tmp();
    write_json(
        &t.file,
        &json!({"version": 1, "docs": {"localIndexEnabled": true}, "other": {"preserved": "yes"},
                "instances": [public("dev", json!({"default": true}))]}),
    );
    let r = reg(&t);
    let doc = r.load().unwrap();
    assert_eq!(doc["version"], json!(1));
    assert_eq!(doc["docs"]["localIndexEnabled"], json!(true));
    assert_eq!(doc["other"]["preserved"], json!("yes"));
    assert_eq!(r.list().unwrap().len(), 1);
    assert_eq!(r.get("dev").unwrap()["clientId"], json!("dev-public-client"));
}

#[test]
fn empty_document_when_missing() {
    let t = tmp();
    let r = reg(&t);
    assert_eq!(r.load().unwrap(), json!({"version": 1, "instances": []}));
    assert!(r.list().unwrap().is_empty());
    assert!(r.get_default().unwrap().is_none());
}

#[test]
fn rejects_multiple_defaults_and_bad_types() {
    let t = tmp();
    write_json(
        &t.file,
        &json!({"version": 1, "instances": [public("dev", json!({"default": true})), public("prod", json!({"default": true}))]}),
    );
    assert_eq!(code(reg(&t).load()), "REGISTRY_RELOAD_FAILED");
    write_json(&t.file, &json!({"version": 1, "instances": [public("broken", json!({"clientId": 42}))]}));
    let r = reg(&t);
    assert_eq!(code(r.load()), "REGISTRY_RELOAD_FAILED");
    assert_eq!(code(r.reload()), "REGISTRY_RELOAD_FAILED");
}

#[test]
fn duplicates_https_and_loopback() {
    let t = tmp();
    let r = reg(&t);
    register(&r, public("dev", json!({}))).unwrap();
    assert_eq!(code(register(&r, public("dev", json!({"clientId": "other"})))), "INSTANCE_ALREADY_EXISTS");
    assert_eq!(code(register(&r, public("bad", json!({"url": "http://remote.service-now.com"})))), "INVALID_INSTANCE_CONFIG");
    assert_eq!(register(&r, public("loopback", json!({"url": "http://127.0.0.1:8080"}))).unwrap()["name"], json!("loopback"));
    assert_eq!(code(register(&r, public("remote", json!({"url": "http://localhost.example.com"})))), "INVALID_INSTANCE_CONFIG");
}

#[test]
fn defaults_follow_first_registration_and_make_default() {
    let t = tmp();
    let r = reg(&t);
    register(&r, public("dev", json!({}))).unwrap();
    register(&r, public("prod", json!({}))).unwrap();
    assert_eq!(r.get_default().unwrap().unwrap()["name"], json!("dev"));
    let patch = json!({"description": "Production", "default": true});
    r.update("prod", patch.as_object().unwrap()).unwrap();
    assert_eq!(r.get_default().unwrap().unwrap()["name"], json!("prod"));
    let defaults = |r: &InstanceRegistry| r.list().unwrap().iter().filter(|i| truthy(i.get("default"))).count();
    assert_eq!(defaults(&r), 1);
    r.register(&public("staging", json!({})), RegisterOptions { make_default: true, ..Default::default() })
        .unwrap();
    assert_eq!(r.get_default().unwrap().unwrap()["name"], json!("staging"));
    assert_eq!(defaults(&r), 1);
}

#[test]
fn auth_requirements() {
    let t = tmp();
    let r = reg(&t);
    for bad in [
        json!({"name": "basic", "url": "https://basic.service-now.com", "authType": "basic", "username": "user"}),
        json!({"name": "cc", "url": "https://cc.service-now.com", "authType": "oauth", "grantType": "client_credentials", "credentialRef": "secret/ref"}),
        json!({"name": "ropc", "url": "https://ropc.service-now.com", "authType": "oauth", "grantType": "password", "username": "user", "clientId": "cid", "credentialRef": {"password": "password/ref"}}),
    ] {
        assert_eq!(code(register(&r, bad)), "INVALID_INSTANCE_CONFIG");
    }
    let pw = |n: &str| credential_ref_for(n, CredentialType::Password).unwrap();
    let cs = |n: &str| credential_ref_for(n, CredentialType::ClientSecret).unwrap();
    register(&r, json!({"name": "basic-ok", "url": "https://basic-ok.service-now.com", "authType": "basic", "username": "user", "credentialRef": pw("basic-ok")})).unwrap();
    register(&r, json!({"name": "cc-ok", "url": "https://cc-ok.service-now.com", "authType": "oauth", "grantType": "client_credentials", "clientId": "cid", "credentialRef": cs("cc-ok")})).unwrap();
    register(&r, json!({"name": "ropc-ok", "url": "https://ropc-ok.service-now.com", "authType": "oauth", "grantType": "password", "username": "user", "clientId": "cid",
        "credentialRef": {"password": pw("ropc-ok"), "clientSecret": cs("ropc-ok")}})).unwrap();
}

#[test]
fn primary_url_rules() {
    let t = tmp();
    let r = reg(&t);
    for (n, url) in [
        ("reject-query", "https://query.service-now.com/instance?tenant=secret"),
        ("reject-hash", "https://fragment.service-now.com/instance#section"),
    ] {
        assert_eq!(code(register(&r, public(n, json!({"url": url})))), "INVALID_INSTANCE_CONFIG");
    }
    register(&r, public("path-prefix", json!({"url": "https://path-prefix.service-now.com/foo/bar///"}))).unwrap();
    assert_eq!(r.get("path-prefix").unwrap()["url"], json!("https://path-prefix.service-now.com/foo/bar"));
    register(&r, public("oauth-query", json!({
        "authorizeUrl": "https://oauth-query.service-now.com/authorize?client=cid",
        "tokenUrl": "https://oauth-query.service-now.com/token?audience=sn"}))).unwrap();
}

#[test]
fn reload_rolls_back_on_failure() {
    let t = tmp();
    write_json(&t.file, &json!({"version": 1, "instances": [public("dev", json!({"default": true}))]}));
    let r = reg(&t);
    assert_eq!(r.list().unwrap()[0]["name"], json!("dev"));
    write_json(&t.file, &json!({"version": 1, "instances": [public("prod", json!({"default": true}))]}));
    assert_eq!(r.reload().unwrap()["instances"][0]["name"], json!("prod"));
    let previous = r.list().unwrap();
    write_json(
        &t.file,
        &json!({"version": 1, "instances": [public("prod", json!({"default": true})), public("broken", json!({"default": true}))]}),
    );
    assert_eq!(code(r.reload()), "REGISTRY_RELOAD_FAILED");
    assert_eq!(r.list().unwrap(), previous);
}

#[test]
fn typed_missing_instance_errors() {
    let t = tmp();
    let r = reg(&t);
    for err in [
        r.get("missing").unwrap_err(),
        r.update("missing", &Object::new()).unwrap_err(),
        r.remove("missing", None).unwrap_err(),
    ] {
        assert_eq!(err.code, "INSTANCE_NOT_FOUND");
        assert_eq!(err.details, *json!({"name": "missing"}).as_object().unwrap());
    }
}

#[cfg(unix)]
#[test]
fn private_directory_and_file_modes() {
    use std::os::unix::fs::PermissionsExt;
    let t = tmp();
    let r = InstanceRegistry::at_paths(&t.user, &t.user, Env::from_pairs(&[]));
    register(&r, public("dev", json!({}))).unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(t.user.parent().unwrap()), 0o700);
    assert_eq!(mode(&t.user), 0o600);
}

#[test]
fn write_failure_is_stable() {
    let t = tmp();
    std::fs::write(t.file.parent().unwrap().join("blocker"), "x").unwrap();
    let target = t.file.parent().unwrap().join("blocker/instances.json");
    let r = InstanceRegistry::at_paths(&t.file, &target, Env::from_pairs(&[]));
    let err = register(&r, public("dev", json!({}))).unwrap_err();
    assert_eq!(err.code, "REGISTRY_WRITE_FAILED");
    assert_eq!(err.detail_str("path"), Some(target.to_str().unwrap()));
    assert!(r.list().unwrap().is_empty());
}

#[test]
fn concurrent_registrations_across_objects() {
    let t = tmp();
    std::thread::scope(|s| {
        for i in 0..8 {
            let file = t.file.clone();
            s.spawn(move || {
                let r = InstanceRegistry::at_paths(&file, &file, Env::from_pairs(&[]));
                register(&r, public(&format!("n{i}"), json!({}))).unwrap();
            });
        }
    });
    assert_eq!(reg(&t).list().unwrap().len(), 8);
}

#[test]
fn writes_user_registry_without_touching_legacy() {
    let t = tmp();
    write_json(&t.legacy, &json!({"version": 1, "docs": {"keep": true}, "instances": [public("legacy", json!({}))]}));
    let before = std::fs::read_to_string(&t.legacy).unwrap();
    let r = InstanceRegistry::at_paths(&t.legacy, &t.user, Env::from_pairs(&[]));
    register(&r, public("new", json!({}))).unwrap();
    assert_eq!(std::fs::read_to_string(&t.legacy).unwrap(), before);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&t.user).unwrap()).unwrap();
    assert_eq!(written["docs"]["keep"], json!(true));
    assert_eq!(written["instances"].as_array().unwrap().len(), 2);
}

#[test]
fn update_and_remove_preserve_document() {
    let t = tmp();
    write_json(&t.file, &json!({"version": 1, "docs": {"enabled": true}, "instances": [public("dev", json!({}))]}));
    let r = reg(&t);
    r.update("dev", json!({"description": "Development", "default": true}).as_object().unwrap())
        .unwrap();
    assert_eq!(r.get("dev").unwrap()["description"], json!("Development"));
    r.remove("dev", None).unwrap();
    assert!(r.list().unwrap().is_empty());
    assert_eq!(r.load().unwrap()["docs"]["enabled"], json!(true));
}

#[test]
fn precommit_rejection_prevents_write() {
    let t = tmp();
    let r = reg(&t);
    let mut seen = Vec::new();
    let mut pre = |doc: &Value| {
        seen.push(doc["instances"].as_array().unwrap().len());
        Err(CodedError::new("PRECOMMIT", "no"))
    };
    let err = r
        .register(&public("dev", json!({})), RegisterOptions { precommit: Some(&mut pre), ..Default::default() })
        .unwrap_err();
    assert_eq!(err.code, "PRECOMMIT");
    assert_eq!(seen, vec![1]);
    assert!(!t.file.exists());
}

#[test]
fn oauth_origin_policy() {
    let t = tmp();
    let r = reg(&t);
    register(&r, public("same-origin", json!({
        "authorizeUrl": "https://same-origin.service-now.com/oauth_auth.do",
        "tokenUrl": "https://same-origin.service-now.com/oauth_token.do"}))).unwrap();
    for (field, value) in [
        ("tokenUrl", "https://idp.example.com/token"),
        ("authorizeUrl", "https://idp.example.com/authorize"),
        ("tokenUrl", "https://same-origin.service-now.com.attacker.example/oauth_token.do"),
    ] {
        let err = register(&r, public(&format!("external-{field}"), json!({ field: value }))).unwrap_err();
        assert_eq!(err.code, "INVALID_INSTANCE_CONFIG");
        assert_eq!(err.detail_str("field"), Some(field));
    }
    let names: Vec<_> = r.list().unwrap().iter().map(|i| i["name"].clone()).collect();
    assert_eq!(names, vec![json!("same-origin")]);

    let approved = InstanceRegistry::at_paths(&t.legacy, &t.legacy, Env::from_pairs(&[(TRUSTED_OAUTH_ORIGINS_ENV, "https://idp.example.com")]));
    register(&approved, public("approved", json!({
        "authorizeUrl": "https://idp.example.com/authorize", "tokenUrl": "https://idp.example.com/token"}))).unwrap();
    let err = register(&approved, public("other-port", json!({"tokenUrl": "https://idp.example.com:8443/token"}))).unwrap_err();
    assert_eq!(err.detail_str("field"), Some("tokenUrl"));

    for value in ["*", "https://idp.example.com/path", "http://idp.example.com", "https://user:pw@idp.example.com", "https://idp.example.com?x=1", "nonsense"] {
        let bad = InstanceRegistry::at_paths(&t.user, &t.user, Env::from_pairs(&[(TRUSTED_OAUTH_ORIGINS_ENV, value)]));
        assert_eq!(code(register(&bad, public("bad-allow-list", json!({"tokenUrl": "https://idp.example.com/token"})))), "INVALID_INSTANCE_CONFIG", "{value}");
    }
}

#[test]
fn stored_endpoint_must_stay_approved() {
    let t = tmp();
    write_json(&t.file, &json!({"version": 1, "instances": [public("stored", json!({"tokenUrl": "https://idp.example.com/token"}))]}));
    assert_eq!(code(reg(&t).load()), "REGISTRY_RELOAD_FAILED");
    let r = InstanceRegistry::at_paths(&t.file, &t.file, Env::from_pairs(&[(TRUSTED_OAUTH_ORIGINS_ENV, "https://idp.example.com")]));
    assert_eq!(r.load().unwrap()["instances"][0]["tokenUrl"], json!("https://idp.example.com/token"));
}

#[test]
fn redacts_secret_shaped_keys_but_keeps_token_url_and_refs() {
    let v = redact_secrets(&json!({
        "password": "p", "clientSecret": "s", "apiKey": "k", "nested": {"accessToken": "t", "ok": 1},
        "tokenUrl": "https://x/token", "credentialRef": "keychain:instance/dev/password"
    }));
    assert!(v.get("password").is_none() && v.get("clientSecret").is_none() && v.get("apiKey").is_none());
    assert!(v["nested"].get("accessToken").is_none());
    assert_eq!(v["nested"]["ok"], json!(1));
    assert_eq!(v["tokenUrl"], json!("https://x/token"));
    assert_eq!(v["credentialRef"], json!("keychain:instance/dev/password"));
}
