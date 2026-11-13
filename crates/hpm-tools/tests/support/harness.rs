//! Tool-golden replay (plan W3 §1 G4). For each `fixtures/golden/tools/<family>/*.json`:
//! fresh stub state and sandbox, a dispatcher built from the caller's registration
//! function, the real MCP stdio handler over an in-memory pipe, then every
//! `steps[].request` replayed and compared with the golden after
//! `NORMALIZATION.json` scrubbing: `response`, `notifications`, `files`, and the
//! ordered `outbound` list (method, path, query pairs, rawQuery, body/form,
//! credential scheme, cookie and user-token flags; headers are transport detail and
//! are not compared).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use base64::Engine;
use hpm_protocol::McpHandler;
use hpm_tools::dispatcher::{Dispatcher, Services};
use hpm_tools::registration::{RegistrationError, ToolRegistry};
use hpm_tools::schema::CompiledSchemaValidator;
use hpm_tools::single_user::{AllowAll, PassThrough, StderrAudit, UnloadedClassification};
use hpm_types::BoxFut;
use hpm_types::context::{AdmissionKey, Principal, RequestContext, TrustedRoot};
use hpm_types::credential::{
    Credential, CredentialBinding, CredentialError, CredentialKind, CredentialNeed,
    CredentialProvider, Secret, UiSession, UiSessionScope,
};
use hpm_types::operation::ScriptOperandGuard;
use hpm_types::registry::{
    DefaultSource, InstanceEntry, RegistryChange, RegistryError, RegistryStore, RegistryView,
};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::scenarios::{self, Endpoints, FAKE_PASSWORD, FAKE_USER, Scenario};
use super::scrub::Scrubber;
use super::stub_sn::{DEFAULT_DOCS_ROUTES, RawStub, StubSn};

/// MCP protocol version the 6.2.0 capture negotiated (`lib/rpc.mjs`).
const PROTOCOL_VERSION: &str = "2025-06-18";
/// `step.settleMs` default: trailing outbound requests land before slicing.
const SETTLE: Duration = Duration::from_millis(50);
const STEP_TIMEOUT: Duration = Duration::from_secs(120);

/// One key per test process: `mint` succeeds once.
static KEY: LazyLock<AdmissionKey> =
    LazyLock::new(|| AdmissionKey::mint().expect("first AdmissionKey mint in this process"));

/// How a lane puts its tools in front of the harness. Called once per golden with a
/// fresh registry and that golden's sandbox.
pub type Register = fn(&mut ToolRegistry, &Sandbox) -> Result<(), RegistrationError>;

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn tools_dir() -> PathBuf {
    repo_root().join("fixtures/golden/tools")
}

/// Sorted golden files of one family directory (`crud`, `itsm-ops`, ...).
pub fn golden_files(family: &str) -> Vec<PathBuf> {
    let dir = tools_dir().join(family);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.expect("golden dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
}

/// Stubs and temp root shared by the goldens of one test (`createWorld`).
pub struct World {
    pub stub: StubSn,
    pub raw: RawStub,
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn path_text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

impl World {
    pub async fn start() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("hpm-golden-")
            .tempdir()
            .expect("temp root");
        // Unix: resolve `/var` → `/private/var` like `fs.realpathSync`. Windows keeps
        // the plain form (canonicalize would add a `\\?\` prefix).
        #[cfg(unix)]
        let root = tmp.path().canonicalize().expect("canonical temp root");
        #[cfg(not(unix))]
        let root = tmp.path().to_path_buf();
        Self {
            stub: StubSn::start().await,
            raw: RawStub::start(DEFAULT_DOCS_ROUTES).await,
            _tmp: tmp,
            root,
        }
    }

    /// `literalsFor(world)`: ephemeral values → placeholders.
    fn literals(&self) -> Vec<(String, String)> {
        let port = self.stub.port;
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{FAKE_USER}:{FAKE_PASSWORD}"));
        let mut lits = vec![
            (self.stub.origin(), "{{INSTANCE_URL}}".to_owned()),
            (
                format!("http://localhost:{port}"),
                "{{INSTANCE_URL_ALT}}".into(),
            ),
            (format!("127.0.0.1:{port}"), "{{INSTANCE_HOST}}".into()),
            (format!("localhost:{port}"), "{{INSTANCE_HOST_ALT}}".into()),
            (self.raw.base(), "{{RAW_GITHUB_URL}}".into()),
            (
                format!("127.0.0.1:{}", self.raw.port),
                "{{RAW_GITHUB_HOST}}".into(),
            ),
            (FAKE_PASSWORD.into(), "{{FAKE_PASSWORD}}".into()),
            (basic, "{{BASIC_CREDENTIAL}}".into()),
            (path_text(&self.root), "{{TMP}}".into()),
        ];
        let repo = repo_root();
        lits.push((path_text(&repo), "{{REPO}}".into()));
        if let Ok(real) = repo.canonicalize() {
            lits.push((path_text(&real), "{{REPO}}".into()));
        }
        let os_tmp = path_text(&std::env::temp_dir());
        let os_tmp = os_tmp.trim_end_matches(['/', '\\']).to_owned();
        lits.push((os_tmp, "{{OS_TMPDIR}}".into()));
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            lits.push((home.to_string_lossy().into_owned(), "{{REAL_HOME}}".into()));
        }
        lits
    }

    /// Placeholder → live value, for golden requests that carry literals.
    fn unscrub(&self, text: &str) -> String {
        let port = self.stub.port;
        [
            ("{{INSTANCE_URL_ALT}}", format!("http://localhost:{port}")),
            ("{{INSTANCE_URL}}", self.stub.origin()),
            ("{{INSTANCE_HOST_ALT}}", format!("localhost:{port}")),
            ("{{INSTANCE_HOST}}", format!("127.0.0.1:{port}")),
            ("{{RAW_GITHUB_URL}}", self.raw.base()),
            (
                "{{RAW_GITHUB_HOST}}",
                format!("127.0.0.1:{}", self.raw.port),
            ),
            ("{{FAKE_PASSWORD}}", FAKE_PASSWORD.to_owned()),
            ("{{TMP}}", path_text(&self.root)),
        ]
        .iter()
        .fold(text.to_owned(), |t, (p, v)| t.replace(p, v))
    }
}

/// Per-golden isolated directories and configuration (`makeSandbox`). Until the
/// composition root takes explicit configuration, lanes read what they need from
/// here when they build their descriptors in [`Register`].
#[allow(
    dead_code,
    reason = "lane registrations read these; no family is ported yet"
)]
pub struct Sandbox {
    pub dir: PathBuf,
    pub home: PathBuf,
    /// Server cwd: fix scripts land here and are compared as `files`.
    pub work: PathBuf,
    pub config_path: PathBuf,
    pub docs_cache: PathBuf,
    /// The environment the 6.2.0 server ran with (`makeSandbox` env).
    pub env: BTreeMap<String, String>,
    /// Keyring seed: account → secret (service `happy-platform-mcp`).
    pub keyring: Vec<(String, String)>,
    pub stub_origin: String,
    pub raw_github_base: String,
    /// `env-credentials` or `registry-*` (golden `mode`).
    pub mode: String,
    /// Stand-in REST credential provider (Basic, golden user) until lane B lands.
    pub credentials: Arc<SandboxCredentials>,
    registry: Arc<StaticRegistry>,
}

fn make_sandbox(world: &World, id: &str, mode: &str, sc: &Scenario) -> Sandbox {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = world.root.join(safe);
    let home = dir.join("home");
    let work = dir.join("work");
    let cfg = dir.join("cfg");
    let docs_cache = dir.join("docs-cache");
    let tmp = dir.join("tmp");
    // A golden may be replayed more than once per world (self-tests).
    let _ = std::fs::remove_dir_all(&dir);
    for d in [&home, &work, &cfg, &dir.join("xdg"), &docs_cache, &tmp] {
        std::fs::create_dir_all(d).expect("sandbox dir");
    }
    let config_path = cfg.join("instances.json");
    let endpoints = Endpoints {
        origin: world.stub.origin(),
        port: world.stub.port,
    };
    let instances = sc.instances.map(|f| f(&endpoints));
    if let Some(reg) = &instances {
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(reg).expect("registry json"),
        )
        .expect("write registry");
    }
    let mut env: BTreeMap<String, String> = [
        ("HOME", path_text(&home)),
        ("USERPROFILE", path_text(&home)),
        ("TMPDIR", path_text(&tmp)),
        ("XDG_CONFIG_HOME", path_text(&dir.join("xdg"))),
        ("TZ", "UTC".into()),
        ("LANG", "C.UTF-8".into()),
        ("NO_COLOR", "1".into()),
        ("HAPPY_CONFIG_PATH", path_text(&config_path)),
        ("HAPPY_DOCS_CACHE_DIR", path_text(&docs_cache)),
        ("GOLDEN_KEYRING_FILE", path_text(&dir.join("keyring.json"))),
        ("GOLDEN_RAW_GITHUB_BASE", world.raw.base()),
        ("SERVICENOW_TOKEN_STORE", "file".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if instances.is_none() {
        env.insert("SERVICENOW_INSTANCE_URL".into(), world.stub.origin());
        env.insert("SERVICENOW_USERNAME".into(), FAKE_USER.into());
        env.insert("SERVICENOW_PASSWORD".into(), FAKE_PASSWORD.into());
    }
    for (k, v) in &sc.env {
        env.insert((*k).to_owned(), (*v).to_owned());
    }
    Sandbox {
        registry: Arc::new(StaticRegistry::new(
            &world.stub.origin(),
            instances.as_ref(),
        )),
        dir,
        home,
        work,
        config_path,
        docs_cache,
        env,
        keyring: sc
            .keyring
            .iter()
            .map(|(a, s)| ((*a).to_owned(), (*s).to_owned()))
            .collect(),
        stub_origin: world.stub.origin(),
        raw_github_base: world.raw.base(),
        mode: mode.to_owned(),
        credentials: Arc::new(SandboxCredentials),
    }
}

/// Read-only registry view built from the sandbox: env mode has the one instance
/// `default`; registry modes take the file's instances and its `default: true` entry.
struct StaticRegistry(Arc<RegistryView>);

impl StaticRegistry {
    fn new(origin: &str, file: Option<&Value>) -> Self {
        let entry = |name: &str, url: &str| InstanceEntry {
            instance_id: name.to_owned(),
            name: name.to_owned(),
            canonical_url: url.to_owned(),
            auth: json!({}),
            config_version: 1,
        };
        let (instances, selected_default, default_source) = match file {
            None => (
                vec![entry("default", origin)],
                Some("default".to_owned()),
                DefaultSource::Env,
            ),
            Some(reg) => {
                let list = reg["instances"].as_array().cloned().unwrap_or_default();
                let entries: Vec<InstanceEntry> = list
                    .iter()
                    .map(|i| {
                        entry(
                            i["name"].as_str().unwrap_or(""),
                            i["url"].as_str().unwrap_or(""),
                        )
                    })
                    .collect();
                let default = list
                    .iter()
                    .find(|i| i["default"] == json!(true))
                    .or(list.first())
                    .and_then(|i| i["name"].as_str())
                    .map(str::to_owned);
                (entries, default, DefaultSource::Registry)
            }
        };
        Self(Arc::new(RegistryView {
            registry_version: 1,
            instances,
            selected_default,
            default_source,
        }))
    }
}

impl RegistryStore for StaticRegistry {
    fn snapshot_for(&self, _principal: &Principal) -> Arc<RegistryView> {
        self.0.clone()
    }
    fn apply<'a>(
        &'a self,
        _expected_version: u64,
        _change: RegistryChange,
    ) -> BoxFut<'a, Result<u64, RegistryError>> {
        Box::pin(async { Err(RegistryError::ReadOnly) })
    }
}

/// Basic credentials of the golden user for whichever instance the call resolved.
/// UI sessions are not wired (scripts/update-set goldens need lane B's provider).
pub struct SandboxCredentials;

impl SandboxCredentials {
    fn no_ui() -> CredentialError {
        CredentialError::Backend("golden harness: UI sessions need the real provider".into())
    }
}

impl CredentialProvider for SandboxCredentials {
    fn prepare<'a>(
        &'a self,
        ctx: &'a RequestContext,
        need: CredentialNeed,
    ) -> BoxFut<'a, Result<Option<CredentialBinding>, CredentialError>> {
        Box::pin(async move {
            if need == CredentialNeed::None {
                return Ok(None);
            }
            let instance = ctx.instance().ok_or_else(|| {
                CredentialError::Backend("golden harness: no instance resolved".into())
            })?;
            Ok(Some(CredentialBinding::new(
                ctx.principal().clone(),
                instance.instance_id.clone(),
                instance.config_version,
                1,
                None,
                format!("golden:{}", instance.instance_id),
            )))
        })
    }
    fn authorize<'a>(
        &'a self,
        binding: &'a CredentialBinding,
    ) -> BoxFut<'a, Result<Credential, CredentialError>> {
        Box::pin(async move {
            let token = base64::engine::general_purpose::STANDARD
                .encode(format!("{FAKE_USER}:{FAKE_PASSWORD}"));
            Ok(Credential {
                kind: CredentialKind::Authorization(Secret::new(format!("Basic {token}"))),
                generation: binding.generation(),
                expires_at: None,
            })
        })
    }
    fn invalidate<'a>(
        &'a self,
        _binding: &'a CredentialBinding,
    ) -> BoxFut<'a, Result<(), CredentialError>> {
        Box::pin(async { Ok(()) })
    }
    fn ui_session<'a>(
        &'a self,
        _binding: &'a CredentialBinding,
        _scope: UiSessionScope,
    ) -> BoxFut<'a, Result<UiSession, CredentialError>> {
        Box::pin(async { Err(Self::no_ui()) })
    }
    fn end_ui_session<'a>(
        &'a self,
        _session: UiSession,
    ) -> BoxFut<'a, Result<(), CredentialError>> {
        Box::pin(async { Err(Self::no_ui()) })
    }
}

/// `listFiles(work)`: every file under `dir` as `{ "rel/posix/path": content }`.
fn list_files(dir: &Path) -> Value {
    fn walk(d: &Path, rel: &str, out: &mut Map<String, Value>) {
        let Ok(entries) = std::fs::read_dir(d) else {
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if e.path().is_dir() {
                walk(&e.path(), &r, out);
            } else {
                let text = std::fs::read(e.path()).unwrap_or_default();
                out.insert(
                    r,
                    Value::String(String::from_utf8_lossy(&text).into_owned()),
                );
            }
        }
    }
    let mut out = Map::new();
    walk(dir, "", &mut out);
    Value::Object(out)
}

/// Minimal newline-delimited JSON-RPC client (`lib/rpc.mjs`).
struct Client {
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    next_id: u64,
}

impl Client {
    async fn send(&mut self, msg: &Value) -> Result<(), String> {
        let mut line = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        line.push('\n');
        self.write
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write to server: {e}"))
    }

    /// Sends a request; returns the response and the messages that arrived before it.
    async fn request(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<(Value, Vec<Value>), String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let mut notifications = Vec::new();
        let read = async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .map_err(|e| format!("read from server: {e}"))?
                    .ok_or_else(|| format!("server closed before answering {method}"))?;
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let is_response = msg.get("id") == Some(&json!(id))
                    && (msg.get("result").is_some() || msg.get("error").is_some());
                if is_response {
                    return Ok::<Value, String>(msg);
                }
                notifications.push(msg);
            }
        };
        let msg = tokio::time::timeout(STEP_TIMEOUT, read)
            .await
            .map_err(|_| format!("timeout waiting for {method}"))??;
        Ok((msg, notifications))
    }
}

fn strip_id(mut msg: Value) -> Value {
    if let Some(o) = msg.as_object_mut() {
        o.remove("id");
        o.remove("jsonrpc");
    }
    msg
}

/// Goldens are platform-neutral: on Windows fold the `\` separators of paths that
/// follow `{{TMP}}` (as `golden_cli.rs` does).
#[cfg(windows)]
fn fold_tmp(v: Value) -> Value {
    fn fold(text: &str) -> String {
        const MARK: &str = "{{TMP}}";
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(MARK) {
            out.push_str(&rest[..at + MARK.len()]);
            rest = &rest[at + MARK.len()..];
            let end = rest
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, ')' | '"' | '\'' | ',' | ']' | '}')
                })
                .unwrap_or(rest.len());
            out.push_str(&rest[..end].replace('\\', "/"));
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }
    match v {
        Value::String(s) => Value::String(fold(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(fold_tmp).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (fold(&k), fold_tmp(v)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(not(windows))]
fn fold_tmp(v: Value) -> Value {
    v
}

/// Outbound entries without `headers` (transport detail, not compared).
fn comparable_outbound(list: &Value) -> Vec<Value> {
    list.as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut e| {
            if let Some(o) = e.as_object_mut() {
                o.remove("headers");
            }
            e
        })
        .collect()
}

fn diff_outbound(step: usize, got: &[Value], want: &[Value], problems: &mut Vec<String>) {
    if got == want {
        return;
    }
    let mut lines = vec![format!(
        "step {step} outbound: {} request(s), golden has {}",
        got.len(),
        want.len()
    )];
    for i in 0..got.len().max(want.len()) {
        match (got.get(i), want.get(i)) {
            (Some(g), Some(w)) if g == w => {}
            (g, w) => lines.push(format!(
                "  [{i}]\n    got:  {}\n    want: {}",
                g.map_or("(none)".into(), Value::to_string),
                w.map_or("(none)".into(), Value::to_string)
            )),
        }
    }
    problems.push(lines.join("\n"));
}

/// Replays one golden file. `Err` carries the per-golden report.
pub async fn replay(world: &World, file: &Path, register: Register) -> Result<(), String> {
    let name = file.strip_prefix(tools_dir()).map_or_else(
        |_| path_text(file),
        |p| p.to_string_lossy().replace('\\', "/"),
    );
    let text = std::fs::read_to_string(file).map_err(|e| format!("{name}: {e}"))?;
    let golden: Value = serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))?;
    let id = golden["id"].as_str().unwrap_or_default();
    let mode = golden["mode"].as_str().unwrap_or("env-credentials");
    let steps = golden["steps"].as_array().cloned().unwrap_or_default();
    let sc = scenarios::scenario(id);

    // Requests to send: the scenario's originals where the capture normalized them,
    // else the golden request with literal placeholders mapped back.
    let mut requests = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let method = step["request"]["method"].as_str().unwrap_or("tools/call");
        let params = match sc.requests.as_ref().and_then(|r| r.get(i)) {
            Some(p) => p.clone(),
            None => {
                let raw = world.unscrub(&step["request"]["params"].to_string());
                if raw.contains("{{") {
                    return Err(format!(
                        "{name}: step {i}: request holds a normalized placeholder; add its original arguments to scenarios.rs"
                    ));
                }
                serde_json::from_str(&raw).map_err(|e| format!("{name}: {e}"))?
            }
        };
        requests.push((method.to_owned(), params));
    }

    let sandbox = make_sandbox(world, id, mode, &sc);
    let mut registry = ToolRegistry::new().map_err(|e| format!("{name}: {e}"))?;
    register(&mut registry, &sandbox).map_err(|e| format!("{name}: registration: {e}"))?;
    let missing: Vec<String> = requests
        .iter()
        .filter(|(m, _)| m == "tools/call")
        .filter_map(|(_, p)| p["name"].as_str())
        .filter(|t| !registry.tools().iter().any(|d| d.name == *t))
        .map(str::to_owned)
        .collect();
    if !missing.is_empty() {
        return Err(format!("{name}: not ported: {}", missing.join(", ")));
    }

    let services = Services {
        validator: Arc::new(
            CompiledSchemaValidator::new(registry.tools()).map_err(|e| format!("{name}: {e}"))?,
        ),
        classification: Arc::new(UnloadedClassification),
        policy: Arc::new(AllowAll),
        protector: Arc::new(PassThrough),
        credentials: sandbox.credentials.clone(),
        audit: Arc::new(StderrAudit),
        script_guard: ScriptOperandGuard {
            shared_account: false,
        },
    };
    let dispatcher = Arc::new(Dispatcher::new(registry, services, *KEY));
    let handler = McpHandler::builder(TrustedRoot::single_user_stdio(), dispatcher)
        .registry(sandbox.registry.clone())
        .build();

    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = tokio::spawn(hpm_protocol::stdio::serve_io(
        handler,
        server_read,
        server_write,
    ));
    let (client_read, client_write) = tokio::io::split(client_io);
    let mut client = Client {
        write: client_write,
        lines: BufReader::new(client_read).lines(),
        next_id: 0,
    };

    world.stub.reset(sc.stub.clone());
    world.raw.take();
    let init = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "golden-capture", "version": "0.0.0"}
    });
    client
        .request("initialize", &init)
        .await
        .map_err(|e| format!("{name}: initialize: {e}"))?;
    client
        .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await
        .map_err(|e| format!("{name}: {e}"))?;

    let mut got_steps = Vec::new();
    for (method, params) in &requests {
        world.stub.take();
        world.raw.take();
        let (message, notifications) = client
            .request(method, params)
            .await
            .map_err(|e| format!("{name}: {e}"))?;
        tokio::time::sleep(SETTLE).await;
        let outbound: Vec<Value> = world
            .stub
            .take()
            .iter()
            .map(|r| r.to_json("servicenow"))
            .chain(world.raw.take().iter().map(|r| r.to_json("github-raw")))
            .collect();
        got_steps.push(json!({
            "outbound": outbound,
            "response": strip_id(message),
            "notifications": notifications,
            "files": list_files(&sandbox.work),
        }));
    }
    drop(client);
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;

    let scrubber = Scrubber::new(world.literals());
    let mut problems = Vec::new();
    for (i, (got, want)) in got_steps.iter().zip(&steps).enumerate() {
        let got = fold_tmp(scrubber.deep(got));
        for field in ["response", "notifications", "files"] {
            if let Some(diff) = first_difference(&got[field], &want[field], String::new()) {
                problems.push(format!("step {i} {field} {diff}"));
            }
        }
        diff_outbound(
            i,
            &comparable_outbound(&got["outbound"]),
            &comparable_outbound(&want["outbound"]),
            &mut problems,
        );
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("{name}\n{}", problems.join("\n")))
    }
}

/// First mismatch as `at <json-pointer>: got X, want Y`; strings show the first
/// differing line so long tool texts stay readable.
fn first_difference(got: &Value, want: &Value, at: String) -> Option<String> {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            let mut keys: Vec<&String> = g.keys().chain(w.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|k| {
                first_difference(
                    g.get(k).unwrap_or(&Value::Null),
                    w.get(k).unwrap_or(&Value::Null),
                    format!("{at}/{k}"),
                )
            })
        }
        (Value::Array(g), Value::Array(w)) if g.len() == w.len() => g
            .iter()
            .zip(w)
            .enumerate()
            .find_map(|(n, (g, w))| first_difference(g, w, format!("{at}/{n}"))),
        (Value::String(g), Value::String(w)) if g != w => {
            let line = g
                .lines()
                .zip(w.lines())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| g.lines().count().min(w.lines().count()));
            Some(format!(
                "at {at} line {}:\n  got:  {:?}\n  want: {:?}",
                line + 1,
                g.lines().nth(line).unwrap_or("<end>"),
                w.lines().nth(line).unwrap_or("<end>"),
            ))
        }
        _ if got != want => Some(format!("at {at}:\n  got:  {got}\n  want: {want}")),
        _ => None,
    }
}

/// Replays every golden of `family` and panics with one report listing each golden
/// that differs (`golden_cli.rs` style).
pub async fn replay_family(family: &str, register: Register) {
    let files = golden_files(family);
    assert!(!files.is_empty(), "no goldens for family {family}");
    let world = World::start().await;
    let mut failures = Vec::new();
    for file in &files {
        if let Err(e) = replay(&world, file, register).await {
            failures.push(e);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} {family} goldens differ:\n\n{}",
        failures.len(),
        files.len(),
        failures.join("\n\n")
    );
}
