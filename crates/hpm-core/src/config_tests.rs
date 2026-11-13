//! Port of `tests/config-path.test.js` and the env-fallback/validation cases of
//! `tests/config-manager.test.js`.

use super::*;
use serde_json::json;

fn obj(v: Value) -> Object {
    v.as_object().unwrap().clone()
}

// --- config-path.test.js ---------------------------------------------------

const HOME: &str = "/example-home";
const LEGACY: &str = "/global/node_modules/happy-platform-mcp/config/servicenow-instances.json";

/// `~/.config/happy-platform-mcp/instances.json` under `HOME`, joined with the platform
/// separator exactly as the resolver does.
fn user() -> PathBuf {
    user_config_path(Path::new(HOME))
}

fn paths(env: &[(&str, &str)], exists: &dyn Fn(&Path) -> bool) -> ConfigPaths {
    resolve_config_paths(
        &Env::from_pairs(env),
        Path::new(HOME),
        Some(Path::new(LEGACY)),
        exists,
    )
}

#[test]
fn expands_windows_home_prefix() {
    assert_eq!(
        expand_home("~\\docs\\instances.json", Path::new(HOME)),
        Path::new(HOME).join("docs\\instances.json")
    );
}

#[test]
fn explicit_path_with_home_expansion() {
    let p = paths(&[("HAPPY_CONFIG_PATH", "~/happy/instances.json")], &|_| {
        false
    });
    let expected = resolve_path(&Path::new(HOME).join("happy/instances.json"));
    assert_eq!(
        p,
        ConfigPaths {
            read_path: expected.clone(),
            write_path: expected,
            source: ConfigSource::Explicit
        }
    );
}

#[test]
fn relative_explicit_path_resolves_against_cwd() {
    let p = paths(&[("HAPPY_CONFIG_PATH", "config/instances.json")], &|_| {
        false
    });
    let expected = std::env::current_dir()
        .unwrap()
        .join("config/instances.json");
    assert_eq!(p.read_path, expected);
    assert_eq!(p.write_path, expected);
}

#[test]
fn user_legacy_and_default_precedence() {
    let user = user();
    let user = user.to_str();
    let found = paths(&[], &|c| Some(c.to_str().unwrap()) == user);
    assert_eq!(
        (found.read_path.to_str(), found.source),
        (user, ConfigSource::User)
    );
    let legacy = paths(&[], &|c| c == Path::new(LEGACY));
    assert_eq!(
        (
            legacy.read_path.to_str(),
            legacy.write_path.to_str(),
            legacy.source
        ),
        (Some(LEGACY), user, ConfigSource::Legacy)
    );
    let none = paths(&[], &|_| false);
    assert_eq!(
        (
            none.read_path.to_str(),
            none.write_path.to_str(),
            none.source
        ),
        (user, user, ConfigSource::User)
    );
}

// --- config-manager.test.js ------------------------------------------------

#[test]
fn client_options_mapping() {
    let opts = instance_to_client_options(&obj(json!({
        "authType": "oauth", "clientId": "cid", "clientSecret": "csec",
        "grantType": "authorization_code", "scope": "useraccount",
        "authorizeUrl": "https://x/oauth_auth.do", "tokenUrl": "https://x/oauth_token.do",
        "redirectPort": 8455, "callbackPath": "/callback",
        "credentialRef": {"password": "keychain:instance/dev/password"}, "password": "p"
    })));
    assert_eq!(opts["redirectPort"], json!(8455));
    assert_eq!(opts["callbackPath"], json!("/callback"));
    assert_eq!(
        opts["credentialRef"]["password"],
        json!("keychain:instance/dev/password")
    );
    assert_eq!(opts["password"], json!("p"));
    assert_eq!(
        instance_to_client_options(&Object::new())["authType"],
        json!("basic")
    );
}

/// A manager whose registry file does not exist, so `.env` fallback applies.
fn manager(env: &[(&str, &str)]) -> (tempfile::TempDir, ConfigManager) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("missing.json");
    let env = Env::from_pairs(env);
    let registry = Arc::new(InstanceRegistry::at_paths(&file, &file, env.clone()));
    (dir, ConfigManager::new(registry, env))
}

const URL: (&str, &str) = ("SERVICENOW_INSTANCE_URL", "https://example.service-now.com");
const OAUTH: (&str, &str) = ("SERVICENOW_AUTH_TYPE", "oauth");

#[test]
fn validate_instance_cases() {
    let (_d, cm) = manager(&[]);
    cm.validate_instance(&json!({
        "name": "public-oauth", "url": "https://example.service-now.com",
        "authType": "oauth", "grantType": "authorization_code", "clientId": "public-client-id"
    }))
    .unwrap();
    let err = cm
        .validate_instance(&json!({
            "name": "bad-ref", "url": "https://bad-ref.service-now.com",
            "authType": "basic", "username": "user", "credentialRef": "plaintext-password"
        }))
        .unwrap_err();
    assert_eq!(err.code, "INVALID_INSTANCE_CONFIG");
}

#[test]
fn basic_auth_env() {
    let (_d, cm) = manager(&[URL]);
    assert!(
        cm.load_from_env()
            .unwrap_err()
            .message
            .contains("Missing ServiceNow credentials")
    );
    let (_d, cm) = manager(&[
        URL,
        ("SERVICENOW_USERNAME", "unit-test-user"),
        ("SERVICENOW_PASSWORD", "unit-test-non-secret"),
    ]);
    let instances = cm.load_from_env().unwrap();
    assert_eq!(instances[0]["username"], json!("unit-test-user"));
    assert_eq!(instances[0]["password"], json!("unit-test-non-secret"));
    assert!(instances[0].get("authType").is_none());
    assert!(instances[0].get("grantType").is_none());
}

#[test]
fn env_url_is_canonicalized_by_registry() {
    let creds = [("SERVICENOW_USERNAME", "u"), ("SERVICENOW_PASSWORD", "p")];
    let (_d, cm) = manager(&[
        (
            "SERVICENOW_INSTANCE_URL",
            "https://example.service-now.com/foo///",
        ),
        creds[0],
        creds[1],
    ]);
    assert_eq!(
        cm.load_from_env().unwrap()[0]["url"],
        json!("https://example.service-now.com/foo")
    );
    let (_d, cm) = manager(&[
        (
            "SERVICENOW_INSTANCE_URL",
            "https://example.service-now.com/foo?tenant=secret",
        ),
        creds[0],
        creds[1],
    ]);
    let msg = cm.load_from_env().unwrap_err().message.to_lowercase();
    assert!(msg.contains("query") || msg.contains("fragment"), "{msg}");
}

#[test]
fn oauth_grants_from_env() {
    let (_d, cm) = manager(&[
        URL,
        OAUTH,
        ("SERVICENOW_OAUTH_GRANT_TYPE", "password"),
        ("SERVICENOW_CLIENT_ID", "cid"),
        ("SERVICENOW_CLIENT_SECRET", "csec"),
    ]);
    assert!(
        cm.load_from_env()
            .unwrap_err()
            .message
            .contains("Missing ServiceNow credentials")
    );

    let (_d, cm) = manager(&[
        URL,
        OAUTH,
        ("SERVICENOW_OAUTH_GRANT_TYPE", "client_credentials"),
        ("SERVICENOW_CLIENT_ID", "cid"),
        ("SERVICENOW_CLIENT_SECRET", "csec"),
        ("SERVICENOW_OAUTH_SCOPE", "useraccount"),
    ]);
    let i = &cm.load_from_env().unwrap()[0];
    assert_eq!(i["username"], json!(""));
    assert_eq!(i["grantType"], json!("client_credentials"));
    assert_eq!(i["scope"], json!("useraccount"));

    let (_d, cm) = manager(&[
        URL,
        OAUTH,
        ("SERVICENOW_OAUTH_GRANT_TYPE", "authorization_code"),
        ("SERVICENOW_CLIENT_ID", "cid"),
        (
            "SERVICENOW_OAUTH_AUTHORIZE_URL",
            "https://example.service-now.com/oauth_auth.do",
        ),
        (
            "SERVICENOW_OAUTH_TOKEN_URL",
            "https://example.service-now.com/oauth_token.do",
        ),
        ("SERVICENOW_OAUTH_REDIRECT_PORT", " 8455 "),
        ("SERVICENOW_OAUTH_CALLBACK_PATH", "/callback"),
    ]);
    let i = &cm.load_from_env().unwrap()[0];
    assert_eq!(i["redirectPort"], json!(8455));
    assert_eq!(
        i["tokenUrl"],
        json!("https://example.service-now.com/oauth_token.do")
    );
}

#[test]
fn rejects_missing_oauth_fields_without_secret_material() {
    let cases: [&[(&str, &str)]; 5] = [
        &[
            ("SERVICENOW_OAUTH_GRANT_TYPE", "client_credentials"),
            ("SERVICENOW_CLIENT_SECRET", "secret-to-protect"),
        ],
        &[
            ("SERVICENOW_OAUTH_GRANT_TYPE", "client_credentials"),
            ("SERVICENOW_CLIENT_ID", "cid"),
        ],
        &[
            ("SERVICENOW_OAUTH_GRANT_TYPE", "password"),
            ("SERVICENOW_CLIENT_ID", "cid"),
            ("SERVICENOW_CLIENT_SECRET", "secret-to-protect"),
        ],
        &[
            ("SERVICENOW_OAUTH_GRANT_TYPE", "password"),
            ("SERVICENOW_CLIENT_ID", "cid"),
            ("SERVICENOW_USERNAME", "oauth-user"),
            ("SERVICENOW_PASSWORD", "oauth-password"),
        ],
        &[("SERVICENOW_OAUTH_GRANT_TYPE", "authorization_code")],
    ];
    for extra in cases {
        let mut env = vec![URL, OAUTH];
        env.extend_from_slice(extra);
        let (_d, cm) = manager(&env);
        let msg = cm.load_from_env().unwrap_err().message;
        assert!(
            msg.contains("Missing ServiceNow credentials"),
            "{extra:?}: {msg}"
        );
        assert!(!msg.contains("secret-to-protect"));
    }
}

#[test]
fn external_idp_requires_trusted_origin() {
    let base = [
        URL,
        OAUTH,
        ("SERVICENOW_OAUTH_GRANT_TYPE", "authorization_code"),
        ("SERVICENOW_CLIENT_ID", "cid"),
        (
            "SERVICENOW_OAUTH_TOKEN_URL",
            "https://login.example.com/token",
        ),
    ];
    let (_d, cm) = manager(&base);
    assert!(
        cm.load_from_env()
            .unwrap_err()
            .message
            .contains("SERVICENOW_OAUTH_TRUSTED_ORIGINS")
    );
    let mut env = base.to_vec();
    env.push((
        "SERVICENOW_OAUTH_TRUSTED_ORIGINS",
        "https://login.example.com",
    ));
    let (_d, cm) = manager(&env);
    assert_eq!(
        cm.load_from_env().unwrap()[0]["tokenUrl"],
        json!("https://login.example.com/token")
    );
}

#[test]
fn rejects_bad_redirect_ports() {
    for value in [
        "12junk",
        "12.5",
        "1e2",
        "0x10",
        "+12",
        "-1",
        "   ",
        "65536",
        "999999999999999999999999999999999999",
        "abc",
    ] {
        let (_d, cm) = manager(&[
            URL,
            OAUTH,
            ("SERVICENOW_OAUTH_GRANT_TYPE", "authorization_code"),
            ("SERVICENOW_CLIENT_ID", "cid"),
            ("SERVICENOW_OAUTH_REDIRECT_PORT", value),
        ]);
        let msg = cm.load_from_env().unwrap_err().message.to_lowercase();
        assert!(msg.contains("redirect port"), "{value:?}");
    }
}

// --- RegistryStore (single user) -------------------------------------------

#[tokio::test]
async fn registry_store_view_and_cas() {
    use hpm_types::registry::{DefaultSource, RegistryChange, RegistryError, RegistryStore};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("instances.json");
    std::fs::write(
        &file,
        json!({"version": 1, "instances": [
            {"name": "dev", "url": "https://dev.service-now.com", "authType": "oauth",
             "grantType": "authorization_code", "clientId": "c", "default": true},
            {"name": "prod", "url": "https://prod.service-now.com", "authType": "oauth",
             "grantType": "authorization_code", "clientId": "c"}]})
        .to_string(),
    )
    .unwrap();
    let env = Env::from_pairs(&[]);
    let cm = ConfigManager::new(
        Arc::new(InstanceRegistry::at_paths(&file, &file, env.clone())),
        env,
    );
    let principal = hpm_types::context::Principal::LocalOperator;
    let view = cm.snapshot_for(&principal);
    assert_eq!(view.instances.len(), 2);
    assert_eq!(view.selected_default.as_deref(), Some("dev"));
    assert_eq!(view.default_source, DefaultSource::Registry);
    let v = view.registry_version;
    assert!(matches!(
        cm.apply(
            v + 1,
            RegistryChange::SetRegistryDefault {
                instance_id: "prod".into()
            }
        )
        .await,
        Err(RegistryError::Conflict { .. })
    ));
    let v2 = cm
        .apply(
            v,
            RegistryChange::SetRegistryDefault {
                instance_id: "prod".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(v2, v + 1);
    assert_eq!(
        cm.snapshot_for(&principal).selected_default.as_deref(),
        Some("prod")
    );
    cm.apply(
        v2,
        RegistryChange::Remove {
            instance_id: "dev".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(cm.snapshot_for(&principal).instances.len(), 1);
}

/// Review L1: concurrent `apply(expected = v)` calls are a true compare-and-swap:
/// exactly one wins, every other one gets `Conflict`.
#[test]
fn concurrent_apply_with_the_same_version_admits_exactly_one() {
    use hpm_types::registry::{RegistryChange, RegistryError, RegistryStore};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("instances.json");
    std::fs::write(
        &file,
        json!({"version": 1, "instances": [
            {"name": "dev", "url": "https://dev.service-now.com", "authType": "oauth",
             "grantType": "authorization_code", "clientId": "c", "default": true},
            {"name": "prod", "url": "https://prod.service-now.com", "authType": "oauth",
             "grantType": "authorization_code", "clientId": "c"}]})
        .to_string(),
    )
    .unwrap();
    let env = Env::from_pairs(&[]);
    let cm = Arc::new(ConfigManager::new(
        Arc::new(InstanceRegistry::at_paths(&file, &file, env.clone())),
        env,
    ));
    let v = cm
        .snapshot_for(&hpm_types::context::Principal::LocalOperator)
        .registry_version;
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let (cm, barrier) = (cm.clone(), barrier.clone());
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                barrier.wait();
                let target = if i % 2 == 0 { "prod" } else { "dev" };
                rt.block_on(cm.apply(
                    v,
                    RegistryChange::SetRegistryDefault {
                        instance_id: target.into(),
                    },
                ))
            })
        })
        .collect();
    let mut won = 0;
    for thread in threads {
        match thread.join().unwrap() {
            Ok(next) => {
                assert_eq!(next, v + 1);
                won += 1;
            }
            Err(RegistryError::Conflict { .. }) => {}
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(won, 1);
}
