//! Replays `fixtures/golden/cli/**` (captured from 6.2.0 by `scripts/golden/stages/cli.mjs`)
//! in-process: temp `HAPPY_CONFIG_PATH`, in-memory keyring, loopback ServiceNow stub.
// Helpers outside `#[test]` fns: fixture I/O failures should abort loudly.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hpm_cli::cli::{Dispatch, route};
use hpm_cli::instance_cli::{
    CliContext, HttpProbe, PromptKind, PromptSpec, Prompter, run_instance_cli,
};
use hpm_core::config::{CodedError, Env};
use hpm_core::keychain::{InstanceCredentialStore, MemorySecretStore, SecretStore};
use hpm_core::registry::InstanceRegistry;
use hpm_core::token_store::{InMemoryTokenStore, TokenStore};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FAKE_PASSWORD: &str = "golden-fake-password-4f9a1c";
const FAKE_CLIENT_SECRET: &str = "golden-fake-client-secret";
const SERVICE: &str = "happy-platform-mcp";

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/cli")
}

/// Loopback stub: `status` for `/api/now/table*`, 200 otherwise.
async fn start_stub(status: u16) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let body = if status == 200 {
                    r#"{"result":[]}"#
                } else {
                    r#"{"error":{"message":"denied"}}"#
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });
    origin
}

fn legacy_registry(origin: &str) -> Value {
    json!({"version": 1, "instances": [
        {"name": "dev", "url": origin, "username": "golden.user", "password": FAKE_PASSWORD, "description": "Golden dev", "default": true},
        {"name": "oauth1", "url": origin, "authType": "oauth", "grantType": "client_credentials", "clientId": "golden-client", "clientSecret": FAKE_CLIENT_SECRET, "description": "Golden oauth"}
    ]})
}

fn ref_registry(origin: &str) -> Value {
    json!({"version": 1, "instances": [
        {"name": "dev", "url": origin, "authType": "basic", "username": "golden.user", "credentialRef": "keychain:instance/dev/password", "description": "Golden dev", "default": true},
        {"name": "oauth1", "url": origin, "authType": "oauth", "grantType": "client_credentials", "clientId": "golden-client", "credentialRef": "keychain:instance/oauth1/client-secret", "description": "Golden oauth"}
    ]})
}

enum Seed {
    None,
    Empty,
    Legacy,
    Ref,
    Raw(&'static str),
}

struct Scenario {
    registry: Seed,
    keyring: bool,
    migrate_target: bool,
    stub_status: u16,
}

/// Mirrors `subprocessScenarios` / `interactiveScenarios` in `cli.mjs`.
fn scenario(id: &str) -> Scenario {
    let mut s = Scenario {
        registry: Seed::None,
        keyring: false,
        migrate_target: false,
        stub_status: 200,
    };
    let with_ref_keyring = [
        "list-credential-ref-registry",
        "test-no-name",
        "test-unknown-instance",
        "test-success",
        "test-http-401",
        "test-http-403",
        "remove-non-tty",
        "update-unknown-option",
        "add-basic",
        "add-duplicate-name",
        "add-invalid-url",
        "add-oauth-client-credentials",
        "update-metadata-flags",
        "update-prompted",
        "update-unknown-instance",
        "update-auth-change-rejected",
        "remove-confirmed",
        "remove-declined",
        "remove-unknown-instance",
        "migrate-nothing-to-do",
    ];
    let ref_only = [
        "test-credential-missing",
        "credential-set-non-tty",
        "credential-set-password",
        "credential-set-client-secret",
        "credential-set-type-prompted",
        "credential-set-wrong-type-for-auth",
        "credential-set-unknown-instance",
    ];
    if with_ref_keyring.contains(&id) {
        s.registry = Seed::Ref;
        s.keyring = true;
    } else if ref_only.contains(&id) {
        s.registry = Seed::Ref;
    }
    match id {
        "list-empty-registry" | "add-into-empty-registry-becomes-default" => {
            s.registry = Seed::Empty
        }
        "list-legacy-plaintext-registry" | "migrate-non-tty" | "migrate-source-equals-target" => {
            s.registry = Seed::Legacy
        }
        "migrate-confirmed" | "migrate-declined" => {
            s.registry = Seed::Legacy;
            s.migrate_target = true;
        }
        "list-registry-malformed-json" => s.registry = Seed::Raw("{ not json"),
        "test-http-401" => s.stub_status = 401,
        "test-http-403" => s.stub_status = 403,
        _ => {}
    }
    if id == "migrate-nothing-to-do" {
        s.migrate_target = true;
    }
    s
}

/// Injected prompts answering by name (`cli-runner.mjs`).
struct Scripted {
    answers: serde_json::Map<String, Value>,
    calls: Arc<Mutex<Vec<Value>>>,
}

impl Prompter for Scripted {
    fn injected(&self) -> BTreeSet<PromptKind> {
        [
            PromptKind::Input,
            PromptKind::Password,
            PromptKind::Select,
            PromptKind::Confirm,
        ]
        .into()
    }
    fn ask(&mut self, kind: PromptKind, spec: &PromptSpec) -> Result<Option<Value>, CodedError> {
        let value = self
            .answers
            .get(&spec.name)
            .cloned()
            .or_else(|| spec.default.clone());
        let kind_name = match kind {
            PromptKind::Input => "input",
            PromptKind::Password => "password",
            PromptKind::Select => "select",
            PromptKind::Confirm => "confirm",
        };
        let mut call = json!({
            "kind": kind_name,
            "name": spec.name,
            "message": spec.message,
            "hasMask": spec.mask,
            "hasDefault": spec.default.is_some(),
        });
        let answered = if kind == PromptKind::Password {
            Some(json!("<masked>"))
        } else {
            value.clone()
        };
        if let Some(a) = answered {
            call["answered"] = a;
        }
        self.calls.lock().unwrap().push(call);
        Ok(if kind == PromptKind::Password {
            Some(json!(FAKE_PASSWORD))
        } else {
            value
        })
    }
}

/// Non-TTY with no injected prompts (subprocess, stdin=/dev/null).
struct NoPrompts;

impl Prompter for NoPrompts {
    fn injected(&self) -> BTreeSet<PromptKind> {
        BTreeSet::new()
    }
    fn ask(&mut self, _: PromptKind, _: &PromptSpec) -> Result<Option<Value>, CodedError> {
        unreachable!("non-TTY prompts are refused before asking")
    }
}

fn replace_placeholders(value: &Value, origin: &str) -> Value {
    match value {
        Value::String(s) => Value::String(
            s.replace("{{INSTANCE_URL}}", origin)
                .replace("{{FAKE_PASSWORD}}", FAKE_PASSWORD),
        ),
        Value::Array(a) => {
            Value::Array(a.iter().map(|v| replace_placeholders(v, origin)).collect())
        }
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), replace_placeholders(v, origin)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn argv_of(golden: &Value) -> Vec<String> {
    golden["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            v.as_str()
                .unwrap()
                .replace("{{EQ}}<value>", "=hunter2-golden")
        })
        .collect()
}

async fn replay(file: &Path) -> Result<(), String> {
    let golden: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let id = golden["id"].as_str().unwrap();
    let interactive = id.starts_with("cli/interactive/");
    let short = id.rsplit('/').next().unwrap();
    let sc = scenario(short);
    let origin = start_stub(sc.stub_status).await;
    let golden = replace_placeholders(&golden, &origin);

    let tmp = tempfile::tempdir().unwrap();
    let suffix = if interactive {
        "interactive".to_owned()
    } else {
        golden["bin"].as_str().unwrap().to_owned()
    };
    let box_dir = tmp.path().join(format!("cli-{short}-{suffix}"));
    std::fs::create_dir_all(box_dir.join("cfg")).unwrap();
    let registry_path = box_dir.join("cfg/instances.json");
    let seed = match sc.registry {
        Seed::None => None,
        Seed::Empty => {
            Some(serde_json::to_string_pretty(&json!({"version": 1, "instances": []})).unwrap())
        }
        Seed::Legacy => Some(serde_json::to_string_pretty(&legacy_registry(&origin)).unwrap()),
        Seed::Ref => Some(serde_json::to_string_pretty(&ref_registry(&origin)).unwrap()),
        Seed::Raw(raw) => Some(raw.to_owned()),
    };
    if let Some(seed) = &seed {
        std::fs::write(&registry_path, seed).unwrap();
    }
    let home = box_dir.join("home");
    let env = Env::from_pairs(&[
        ("HAPPY_CONFIG_PATH", registry_path.to_str().unwrap()),
        ("HOME", home.to_str().unwrap()),
    ]);
    let mut legacy_path = None;
    let registry = if sc.migrate_target && interactive {
        let legacy = box_dir.join("legacy.json");
        std::fs::copy(&registry_path, &legacy).unwrap();
        std::fs::remove_file(&registry_path).unwrap();
        legacy_path = Some(legacy.clone());
        InstanceRegistry::at_paths(legacy, registry_path.clone(), env.clone())
    } else {
        InstanceRegistry::new(env.clone())
    };

    let memory = Arc::new(MemorySecretStore::new());
    if sc.keyring {
        memory.insert(SERVICE, "keychain:instance/dev/password", FAKE_PASSWORD);
        memory.insert(
            SERVICE,
            "keychain:instance/oauth1/client-secret",
            FAKE_CLIENT_SECRET,
        );
    }
    let secrets: Arc<dyn SecretStore> = memory.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let prompter: Box<dyn Prompter> = if interactive {
        Box::new(Scripted {
            answers: golden["answers"].as_object().cloned().unwrap_or_default(),
            calls: calls.clone(),
        })
    } else {
        Box::new(NoPrompts)
    };

    let argv = argv_of(&golden);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = match route(&argv, &mut stdout, &mut stderr) {
        Dispatch::Exit(code) => code,
        Dispatch::Serve => return Err(format!("{id}: unexpected serve")),
        Dispatch::Instance => {
            let token_store: Arc<dyn TokenStore> = Arc::new(InMemoryTokenStore::new());
            let mut ctx = CliContext {
                registry: Arc::new(registry),
                credentials: InstanceCredentialStore::new(secrets),
                prompter,
                tty: false,
                probe: Box::new(HttpProbe),
                token_stores: vec![("OS keychain", token_store)],
                os_user: "golden".into(),
                stdout: &mut stdout,
                stderr: &mut stderr,
            };
            run_instance_cli(&argv, &mut ctx).await
        }
    };
    let tmp_str = tmp.path().to_str().unwrap();
    let clean = |b: &[u8]| String::from_utf8_lossy(b).replace(tmp_str, "{{TMP}}");

    let mut problems = Vec::new();
    let mut check = |what: &str, got: Value, want: &Value| {
        if &got != want {
            problems.push(format!("{what}:\n  got:  {got}\n  want: {want}"));
        }
    };
    check("exit", json!(code), &golden["exit"]["code"]);
    // Recorded deviation (review M2): `instance migrate` names its plaintext source
    // before confirming; 6.2.0 did not. Only that one line is dropped.
    let stdout_text: String = clean(&stdout)
        .split_inclusive('\n')
        .filter(|l| !l.starts_with(hpm_cli::instance_cli::MIGRATION_SOURCE_PREFIX))
        .collect();
    check("stdout", json!(stdout_text), &golden["stdout"]);
    check("stderr", json!(clean(&stderr)), &golden["stderr"]);
    let after = &golden["after"];
    let registry_json = match std::fs::read_to_string(&registry_path) {
        Err(_) => Value::Null,
        Ok(text) => serde_json::from_str(&text).unwrap_or(json!("unparseable")),
    };
    check("registryJson", registry_json, &after["registryJson"]);
    let accounts = memory.accounts(SERVICE);
    let mut sorted = accounts.clone();
    sorted.sort();
    check("keyringAccounts", json!(sorted), &after["keyringAccounts"]);
    let matches: serde_json::Map<String, Value> = sorted
        .iter()
        .map(|a| {
            let v = memory.value(SERVICE, a).unwrap_or_default();
            (
                a.clone(),
                json!(v == FAKE_PASSWORD || v == FAKE_CLIENT_SECRET),
            )
        })
        .collect();
    check(
        "keyringValueMatchesFakeSecret",
        Value::Object(matches),
        &after["keyringValueMatchesFakeSecret"],
    );
    if let Some(legacy) = legacy_path {
        check(
            "legacyFileUntouched",
            json!(legacy.exists()),
            &after["legacyFileUntouched"],
        );
    }
    if interactive {
        check(
            "promptCalls",
            Value::Array(calls.lock().unwrap().clone()),
            &golden["promptCalls"],
        );
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("{id}\n{}", problems.join("\n")))
    }
}

fn golden_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in [fixtures_dir(), fixtures_dir().join("interactive")] {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "json") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[tokio::test]
async fn cli_goldens_replay() {
    let files = golden_files();
    assert!(files.len() >= 60, "expected the full CLI golden set");
    let mut failures = Vec::new();
    for file in &files {
        if let Err(e) = replay(file).await {
            failures.push(e);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} goldens differ:\n\n{}",
        failures.len(),
        files.len(),
        failures.join("\n\n")
    );
}
