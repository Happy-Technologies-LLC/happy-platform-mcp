//! Tool-golden replay (plan W3 §1 G4). For each `fixtures/golden/tools/<family>/*.json`:
//! fresh stub state and sandbox, the real single-user composition
//! (`hpm_cli::SingleUser`: every family's `register`, `SingleUserCredentialProvider`,
//! pinned clients) pointed at the loopback stubs, the real MCP stdio handler over an
//! in-memory pipe, then every `steps[].request` replayed and compared with the
//! golden after `NORMALIZATION.json` scrubbing: `response`, `notifications`,
//! `files`, the ordered `outbound` list (method, path, query pairs, rawQuery,
//! body/form, credential scheme, cookie and user-token flags; other headers are
//! transport detail), and which instance (`Host`) each ServiceNow request reached.
//!
//! Handlers run in this process, so they must take every path from the composition
//! (`FamilyDeps::working_dir`, `docs_base_url`, the `ConfigManager` env), never from
//! `std::env` or the cwd: the `files` check only sees the sandbox `work` dir.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use base64::Engine;
use hpm_cli::SingleUser;
use hpm_core::client::ClientDeps;
use hpm_core::client::RawError;
use hpm_core::client::oauth_code::{AuthCodeFlow, AuthCodeFlowConfig, TokenPoster};
use hpm_core::config::{ConfigManager, Env};
use hpm_core::keychain::{MemorySecretStore, SERVICE_NAME};
use hpm_core::registry::InstanceRegistry;
use hpm_protocol::McpHandler;
use hpm_tools::dispatcher::Dispatcher;
use hpm_types::BoxFut;
use hpm_types::context::{AdmissionKey, TrustedRoot};
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

/// Builds the dispatcher for one golden. [`production`] is the real composition;
/// self-tests pass their own to put test-only tools in front of the harness.
pub type Compose = fn(&Sandbox, AdmissionKey) -> Result<Arc<Dispatcher>, String>;

/// The real single-user composition (`hpm_cli::SingleUser`) over the sandbox.
pub fn production(sandbox: &Sandbox, key: AdmissionKey) -> Result<Arc<Dispatcher>, String> {
    SingleUser::new(sandbox.config.clone(), sandbox.client_deps())
        .docs_base_url(sandbox.raw_github_base.clone())
        .working_dir(sandbox.work.clone())
        .dispatcher(key)
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn tools_dir() -> PathBuf {
    repo_root().join("fixtures/golden/tools")
}

/// Every golden as `<family>/<file>.json`, sorted.
pub fn all_goldens() -> Vec<String> {
    let mut out = Vec::new();
    let mut families: Vec<_> = std::fs::read_dir(tools_dir())
        .expect("tools goldens")
        .map(|e| e.expect("golden dir entry"))
        .filter(|e| e.path().is_dir())
        .collect();
    families.sort_by_key(std::fs::DirEntry::file_name);
    for family in families {
        let mut files: Vec<String> = std::fs::read_dir(family.path())
            .expect("golden family dir")
            .map(|e| {
                e.expect("golden entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|n| n.ends_with(".json"))
            .collect();
        files.sort();
        let family = family.file_name().to_string_lossy().into_owned();
        out.extend(files.into_iter().map(|f| format!("{family}/{f}")));
    }
    out
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
        // the plain form (canonicalize would add a `\\?\` prefix); its long form is
        // added as a literal below.
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

    fn default_host(&self) -> String {
        format!("127.0.0.1:{}", self.stub.port)
    }

    fn prod_host(&self) -> String {
        format!("localhost:{}", self.stub.port)
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
            (self.default_host(), "{{INSTANCE_HOST}}".into()),
            (self.prod_host(), "{{INSTANCE_HOST_ALT}}".into()),
            (self.raw.base(), "{{RAW_GITHUB_URL}}".into()),
            (
                format!("127.0.0.1:{}", self.raw.port),
                "{{RAW_GITHUB_HOST}}".into(),
            ),
            (FAKE_PASSWORD.into(), "{{FAKE_PASSWORD}}".into()),
            (basic, "{{BASIC_CREDENTIAL}}".into()),
        ];
        for form in path_forms(&self.root) {
            lits.push((form, "{{TMP}}".into()));
        }
        let repo = repo_root();
        lits.extend(
            path_forms(&repo)
                .into_iter()
                .map(|f| (f, "{{REPO}}".into())),
        );
        let os_tmp = path_text(&std::env::temp_dir());
        let os_tmp = os_tmp.trim_end_matches(['/', '\\']);
        lits.extend(
            path_forms(Path::new(os_tmp))
                .into_iter()
                .map(|f| (f, "{{OS_TMPDIR}}".into())),
        );
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
            ("{{INSTANCE_HOST_ALT}}", self.prod_host()),
            ("{{INSTANCE_HOST}}", self.default_host()),
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

/// Spellings of `p` a port may print: as given, canonical, and (Windows) the
/// canonical long form without `\\?\` plus each form JSON-escaped (`\` → `\\`), as it
/// appears inside JSON-stringified tool text.
fn path_forms(p: &Path) -> Vec<String> {
    let mut forms = vec![path_text(p)];
    if let Ok(real) = p.canonicalize() {
        let real = path_text(&real);
        forms.push(
            real.strip_prefix(r"\\?\")
                .map_or(real.clone(), str::to_owned),
        );
    }
    if cfg!(windows) {
        let escaped: Vec<String> = forms.iter().map(|f| f.replace('\\', r"\\")).collect();
        forms.extend(escaped);
    }
    // Longest first so an escaped form wins over its plain prefix.
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    forms.dedup();
    forms
}

/// Per-golden isolated directories and configuration (`makeSandbox`).
pub struct Sandbox {
    /// Server working directory: fix scripts land here and are compared as `files`.
    pub work: PathBuf,
    pub raw_github_base: String,
    /// Registry file + the env the 6.2.0 server ran with.
    pub config: Arc<ConfigManager>,
    /// Keyring seeded from the scenario (service `happy-platform-mcp`).
    pub secrets: Arc<MemorySecretStore>,
}

impl Sandbox {
    /// Production client seams over the sandbox keyring, with a sign-in flow that
    /// never opens a browser.
    pub fn client_deps(&self) -> ClientDeps {
        let mut deps =
            ClientDeps::production(self.secrets.clone()).expect("reqwest transport for the replay");
        deps.auth_flow = Arc::new(NoBrowser);
        deps.os_user = FAKE_USER.to_owned();
        deps.trusted_origins = Arc::new(|| None);
        deps
    }
}

/// Interactive OAuth is never part of a golden; fail instead of opening a browser.
struct NoBrowser;

impl AuthCodeFlow for NoBrowser {
    fn run<'a>(
        &'a self,
        _config: AuthCodeFlowConfig,
        _post: Arc<dyn TokenPoster>,
    ) -> BoxFut<'a, Result<Value, RawError>> {
        Box::pin(async { Err(RawError::message("golden harness: interactive sign-in")) })
    }
}

fn make_sandbox(world: &World, id: &str, sc: &Scenario) -> Result<Sandbox, String> {
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
    // `remove_dir_all` below must stay inside the world root.
    if safe.chars().all(|c| c == '.') {
        return Err(format!("unsafe golden id {id:?}"));
    }
    let dir = world.root.join(safe);
    let home = dir.join("home");
    let work = dir.join("work");
    let cfg = dir.join("cfg");
    let docs_cache = dir.join("docs-cache");
    let tmp = dir.join("tmp");
    // A golden may be replayed more than once per world (self-tests).
    let _ = std::fs::remove_dir_all(&dir);
    for d in [&home, &work, &cfg, &dir.join("xdg"), &docs_cache, &tmp] {
        std::fs::create_dir_all(d).map_err(|e| format!("sandbox dir: {e}"))?;
    }
    let config_path = cfg.join("instances.json");
    let endpoints = Endpoints {
        origin: world.stub.origin(),
        port: world.stub.port,
    };
    let instances = sc.instances.map(|f| f(&endpoints));
    if let Some(reg) = &instances {
        let text = serde_json::to_string_pretty(reg).map_err(|e| e.to_string())?;
        std::fs::write(&config_path, text).map_err(|e| format!("write registry: {e}"))?;
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
    let env = Env::from_fn(move |k| env.get(k).cloned());
    let registry = InstanceRegistry::at_paths(&config_path, &config_path, env.clone());
    let secrets = Arc::new(MemorySecretStore::new());
    for (account, secret) in &sc.keyring {
        secrets.insert(SERVICE_NAME, account, secret);
    }
    Ok(Sandbox {
        work,
        raw_github_base: world.raw.base(),
        config: Arc::new(ConfigManager::new(Arc::new(registry), env)),
        secrets,
    })
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

    async fn next_message(&mut self) -> Result<Option<Value>, String> {
        let Some(line) = self
            .lines
            .next_line()
            .await
            .map_err(|e| format!("read from server: {e}"))?
        else {
            return Ok(None);
        };
        serde_json::from_str(&line)
            .map(Some)
            .map_err(|_| format!("non-JSON line from server: {line:?}"))
    }

    /// Sends a request; returns the response and the messages that arrived before it
    /// (`notifications.slice(mark)`).
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
                let msg = self
                    .next_message()
                    .await?
                    .ok_or_else(|| format!("server closed before answering {method}"))?;
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

    /// Messages that arrive after a response belong to no step (JS slices from the
    /// next send); read and drop what is already buffered.
    async fn drain(&mut self) -> Result<(), String> {
        while let Ok(next) = tokio::time::timeout(Duration::ZERO, self.next_message()).await {
            if next?.is_none() {
                break;
            }
        }
        Ok(())
    }
}

fn strip_id(mut msg: Value) -> Value {
    if let Some(o) = msg.as_object_mut() {
        o.remove("id");
        o.remove("jsonrpc");
    }
    msg
}

/// Goldens are platform-neutral: on Windows fold each run of `\` in the path that
/// follows `{{TMP}}` into one `/` (as `golden_cli.rs` `normalize_tmp`). A run right
/// before `"` is a JSON escape of the quote and stays.
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
            let path = &rest[..end];
            let trailing = path.len() - path.trim_end_matches('\\').len();
            let quote_escape = trailing > 0 && rest[end..].starts_with('"');
            let body = if quote_escape {
                &path[..path.len() - trailing]
            } else {
                path
            };
            let mut prev_backslash = false;
            for c in body.chars() {
                if c == '\\' {
                    if !prev_backslash {
                        out.push('/');
                    }
                    prev_backslash = true;
                } else {
                    out.push(c);
                    prev_backslash = false;
                }
            }
            if quote_escape {
                out.push_str(&path[path.len() - trailing..]);
            }
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

/// Intentional 7.0 difference (RFC §4 per-operation UI logout): Rust ends every
/// cookie UI session with `GET /logout.do` as the operation's last request; 6.2.0
/// never logged out, so the goldens do not record it. Only that exact request,
/// and only as the final outbound entry of a step, is left out of the comparison;
/// a logout anywhere else (e.g. before a later read) still shows as a mismatch.
pub fn without_final_ui_logout(mut got: Vec<Value>) -> Vec<Value> {
    let is_ui_logout = got.last().is_some_and(|e| {
        e["target"] == "servicenow"
            && e["method"] == "GET"
            && e["path"] == "/logout.do"
            && e["credentials"]["sessionCookie"] == true
    });
    if is_ui_logout {
        got.pop();
    }
    got
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

/// Display name of a golden: `<family>/<file>.json` with `/` on every OS.
fn golden_name(file: &Path) -> String {
    file.strip_prefix(tools_dir()).map_or_else(
        |_| path_text(file),
        |p| p.to_string_lossy().replace('\\', "/"),
    )
}

/// Replays one golden through the real composition.
pub async fn replay(world: &World, golden: &str) -> Result<(), String> {
    replay_with(world, &tools_dir().join(golden), production).await
}

/// Replays one golden file through `compose`. `Err` carries the per-golden report;
/// a tool the composition does not register fails with `not ported: <tool>`.
pub async fn replay_with(world: &World, file: &Path, compose: Compose) -> Result<(), String> {
    let name = golden_name(file);
    let text = std::fs::read_to_string(file).map_err(|e| format!("{name}: {e}"))?;
    let golden: Value = serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))?;
    let id = golden["id"].as_str().unwrap_or_default();
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

    let sandbox = make_sandbox(world, id, &sc).map_err(|e| format!("{name}: {e}"))?;
    let dispatcher = compose(&sandbox, *KEY).map_err(|e| format!("{name}: {e}"))?;
    let mut missing: Vec<&str> = requests
        .iter()
        .filter(|(m, _)| m == "tools/call")
        .filter_map(|(_, p)| p["name"].as_str())
        .filter(|t| dispatcher.get_tool(t).is_none())
        .collect();
    missing.dedup();
    if !missing.is_empty() {
        return Err(format!("{name}: not ported: {}", missing.join(", ")));
    }
    let handler = McpHandler::builder(TrustedRoot::single_user_stdio(), dispatcher)
        .registry(sandbox.config.clone())
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
    let mut hosts: Vec<Vec<String>> = Vec::new();
    for (method, params) in &requests {
        world.stub.take();
        world.raw.take();
        client.drain().await.map_err(|e| format!("{name}: {e}"))?;
        let (message, notifications) = client
            .request(method, params)
            .await
            .map_err(|e| format!("{name}: {e}"))?;
        tokio::time::sleep(SETTLE).await;
        let sn = world.stub.take();
        hosts.push(sn.iter().map(|r| r.host.clone()).collect());
        let outbound: Vec<Value> = sn
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
            &without_final_ui_logout(comparable_outbound(&got["outbound"])),
            &comparable_outbound(&want["outbound"]),
            &mut problems,
        );
        let expected = if sc.prod_steps.contains(&i) {
            world.prod_host()
        } else {
            world.default_host()
        };
        if let Some(wrong) = hosts[i].iter().find(|h| **h != expected) {
            problems.push(format!(
                "step {i} instance: a ServiceNow request reached {}, expected {}",
                scrubber.string(wrong),
                scrubber.string(&expected)
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("{name}\n{}", problems.join("\n")))
    }
}

/// First mismatch as `at <json-pointer>: got X, want Y`; strings show the first
/// differing line so long tool texts stay readable. A key present on one side only
/// is a mismatch even when the other side's value is `null`.
pub fn first_difference(got: &Value, want: &Value, at: String) -> Option<String> {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            let mut keys: Vec<&String> = g.keys().chain(w.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|k| match (g.get(k), w.get(k)) {
                (Some(gv), Some(wv)) => first_difference(gv, wv, format!("{at}/{k}")),
                (Some(gv), None) => Some(format!("at {at}/{k}: only in got: {gv}")),
                (None, Some(wv)) => Some(format!("at {at}/{k}: only in golden: {wv}")),
                (None, None) => None,
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
