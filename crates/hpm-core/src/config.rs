//! Configuration paths and manager (lane A). Ports `src/config-path.js`,
//! `src/config-manager.js`, `src/url-limits.js` and `src/package-info.js`.

use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde_json::{Map, Value};

use hpm_types::BoxFut;
use hpm_types::context::Principal;
use hpm_types::registry::{
    DefaultSource, InstanceEntry, RegistryChange, RegistryError, RegistryStore, RegistryView,
};

use crate::registry::{InstanceRegistry, RegisterOptions, canonicalize_instance_url};

/// `package.json` name and version (`src/package-info.js`).
pub const PACKAGE_NAME: &str = "happy-platform-mcp";
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// JSON object, the shape of every instance record (6.x keeps them as plain objects).
pub type Object = Map<String, Value>;

// ---------------------------------------------------------------------------
// Errors

/// An error with the stable 6.x `code`, message and `details` object. The CLI prints
/// only the code and selected details for `REGISTRY_*` codes, never the message.
#[derive(Clone, Debug, PartialEq)]
pub struct CodedError {
    /// Empty when the JS error had no `code` (plain `Error`/`TypeError`).
    pub code: String,
    pub message: String,
    pub details: Object,
}

impl CodedError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Object::new(),
        }
    }
    /// A code-less error (JS `new Error(message)`).
    pub fn plain(message: impl Into<String>) -> Self {
        Self::new("", message)
    }
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.details.insert(key.to_owned(), value.into());
        self
    }
    pub fn detail_str(&self, key: &str) -> Option<&str> {
        self.details.get(key).and_then(Value::as_str)
    }
}

impl fmt::Display for CodedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

// ---------------------------------------------------------------------------
// Environment

type EnvFn = dyn Fn(&str) -> Option<String> + Send + Sync;

/// Read-only environment lookup, injected so tests never mutate the process
/// environment (`std::env::set_var` is unsafe in edition 2024).
#[derive(Clone)]
pub struct Env(Arc<EnvFn>);

impl Env {
    /// The process environment, read at lookup time (JS reads `process.env` lazily),
    /// then the restricted `.env` values installed by [`install_dotenv`] (P4-3).
    pub fn process() -> Self {
        Self(Arc::new(|key| {
            std::env::var(key)
                .ok()
                .or_else(|| DOTENV.get().and_then(|values| values.get(key).cloned()))
        }))
    }
    pub fn from_fn(f: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Self(Arc::new(move |key| map.get(key).cloned()))
    }
    /// A shared mutable map, for tests that change the environment mid-test.
    pub fn shared(map: Arc<Mutex<HashMap<String, String>>>) -> Self {
        Self(Arc::new(move |key| {
            map.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(key)
                .cloned()
        }))
    }
    pub fn get(&self, key: &str) -> Option<String> {
        (self.0)(key)
    }
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Env(..)")
    }
}

/// The `.env` keys 7.0 loads (owner decision P4-3): the ServiceNow instance and
/// credential keys 6.2.0 documents, plus the HTTP bearer token. Every other key
/// (bind host, `*_ALLOWED_*`, telemetry URL, registry path, ...) is ignored.
pub const DOTENV_KEYS: &[&str] = &[
    "SERVICENOW_INSTANCE",
    "SERVICENOW_INSTANCE_URL",
    "SERVICENOW_USERNAME",
    "SERVICENOW_PASSWORD",
    "SERVICENOW_AUTH_TYPE",
    "SERVICENOW_CLIENT_ID",
    "SERVICENOW_CLIENT_SECRET",
    "SERVICENOW_OAUTH_GRANT_TYPE",
    "HAPPY_MCP_API_TOKEN",
];

static DOTENV: OnceLock<HashMap<String, String>> = OnceLock::new();

/// The accepted values of one `.env` file and the warnings to print (stderr).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DotEnv {
    pub values: HashMap<String, String>,
    pub warnings: Vec<String>,
}

impl DotEnv {
    /// Parses `text` (dotenv syntax: `KEY=value`, optional `export `, `#`
    /// comments, single/double quotes) and keeps only [`DOTENV_KEYS`] that
    /// `is_set` (the real environment) does not already define. Warnings never
    /// quote a line, so a malformed secret is not echoed.
    pub fn parse(text: &str, is_set: impl Fn(&str) -> bool) -> Self {
        let mut out = Self::default();
        // A-4: a UTF-8 BOM is not part of the first key (dotenv's `\s` drops it).
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let Some((key, value)) = line.split_once('=') else {
                out.warnings.push(format!(
                    ".env line {}: ignored (expected KEY=value)",
                    index + 1
                ));
                continue;
            };
            let key = key.trim();
            let valid_key = !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-');
            let Some(value) = valid_key.then(|| dotenv_value(value.trim())).flatten() else {
                out.warnings
                    .push(format!(".env line {}: ignored (malformed)", index + 1));
                continue;
            };
            if !DOTENV_KEYS.contains(&key) || is_set(key) {
                continue;
            }
            out.values.insert(key.to_owned(), value);
        }
        if out.values.contains_key("SERVICENOW_INSTANCE_URL") {
            out.warnings.push(
                "SERVICENOW_INSTANCE_URL was loaded from .env in the working directory; \
                 prefer `happy-platform-mcp instance add`"
                    .to_owned(),
            );
        }
        out
    }
}

/// A dotenv value: quoted (`'..'` literal, `".."` with `\n` escapes) or bare
/// with an optional ` #` comment. `None` for an unterminated quote.
fn dotenv_value(value: &str) -> Option<String> {
    for quote in ['\'', '"'] {
        if let Some(rest) = value.strip_prefix(quote) {
            let end = rest.find(quote)?;
            let inner = &rest[..end];
            return Some(if quote == '"' {
                inner.replace("\\n", "\n")
            } else {
                inner.to_owned()
            });
        }
    }
    let bare = match value.find(" #") {
        Some(i) => &value[..i],
        None => value,
    };
    Some(bare.trim_end().to_owned())
}

/// Loads `.env` from `dir` (the working directory, as 6.2.0 `dotenv.config()`)
/// into the overlay [`Env::process`] reads after the real environment, and
/// returns the warnings to print on stderr. A missing file is not an error. Call
/// once at process start, before any `Env::process` lookup.
pub fn install_dotenv(dir: &Path) -> Vec<String> {
    let text = match read_dotenv(&dir.join(".env")) {
        Ok(Some(text)) => text,
        Ok(None) => return Vec::new(),
        Err(warning) => return vec![warning],
    };
    let DotEnv { values, warnings } = DotEnv::parse(&text, |key| std::env::var_os(key).is_some());
    if DOTENV.set(values).is_err() {
        return vec![".env already loaded".to_owned()];
    }
    warnings
}

/// Largest `.env` read (A-5); a larger file is not loaded at all.
pub const DOTENV_MAX_BYTES: u64 = 64 * 1024;

/// Reads `path` if it is a regular file (a symlink to one is followed, as
/// 6.2.0 does) of at most [`DOTENV_MAX_BYTES`]. `Ok(None)`: no file. On Unix
/// the open is non-blocking, so a FIFO never stalls start-up; it is then
/// refused as not a regular file. Errors are the warning to print.
pub fn read_dotenv(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!(".env not loaded: {e}")),
    };
    let regular = file.metadata().map(|m| m.is_file()).unwrap_or(false);
    if !regular {
        return Err(".env not loaded: not a regular file".to_owned());
    }
    let mut bytes = Vec::new();
    if let Err(e) = file.take(DOTENV_MAX_BYTES + 1).read_to_end(&mut bytes) {
        return Err(format!(".env not loaded: {e}"));
    }
    if bytes.len() as u64 > DOTENV_MAX_BYTES {
        return Err(".env not loaded: larger than 64 KiB".to_owned());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| ".env not loaded: not UTF-8".to_owned())
}

/// `os.homedir()`: `$HOME` (`%USERPROFILE%` on Windows), else the passwd entry.
pub fn home_dir(env: &Env) -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = env.get(var).filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    #[cfg(unix)]
    if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::getuid()) {
        return user.dir;
    }
    PathBuf::from("/")
}

// ---------------------------------------------------------------------------
// url-limits.js

pub const MAX_URL_LENGTH: usize = 4096;

/// JS `value.length` counts UTF-16 code units.
pub fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}

pub fn exceeds_url_length(value: &str) -> bool {
    js_len(value) > MAX_URL_LENGTH
}

pub fn trim_trailing_slashes(value: &str) -> &str {
    value.trim_end_matches('/')
}

pub fn trim_leading_slashes(value: &str) -> &str {
    value.trim_start_matches('/')
}

// ---------------------------------------------------------------------------
// config-path.js

/// Node `path.resolve`: absolute against the working directory, lexically normalized.
pub fn resolve_path(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(p)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn expand_home(input: &str, home_dir: &Path) -> PathBuf {
    if input == "~" {
        return home_dir.to_path_buf();
    }
    let sep = std::path::MAIN_SEPARATOR;
    if input.starts_with(&format!("~{sep}")) || input.starts_with("~/") || input.starts_with("~\\")
    {
        return home_dir.join(&input[2..]);
    }
    PathBuf::from(input)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    Explicit,
    User,
    Legacy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPaths {
    pub read_path: PathBuf,
    pub write_path: PathBuf,
    pub source: ConfigSource,
}

pub fn user_config_path(home_dir: &Path) -> PathBuf {
    home_dir
        .join(".config")
        .join("happy-platform-mcp")
        .join("instances.json")
}

/// Precedence: `HAPPY_CONFIG_PATH` > `~/.config/happy-platform-mcp/instances.json` >
/// legacy `config/servicenow-instances.json` (read only; writes go to the user path).
pub fn resolve_config_paths(
    env: &Env,
    home_dir: &Path,
    legacy_path: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
) -> ConfigPaths {
    if let Some(explicit) = env.get("HAPPY_CONFIG_PATH") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            let resolved = resolve_path(&expand_home(explicit, home_dir));
            return ConfigPaths {
                read_path: resolved.clone(),
                write_path: resolved,
                source: ConfigSource::Explicit,
            };
        }
    }
    let user = user_config_path(home_dir);
    if exists(&user) {
        return ConfigPaths {
            read_path: user.clone(),
            write_path: user,
            source: ConfigSource::User,
        };
    }
    if let Some(legacy) = legacy_path.filter(|p| exists(p)) {
        return ConfigPaths {
            read_path: legacy.to_path_buf(),
            write_path: user,
            source: ConfigSource::Legacy,
        };
    }
    ConfigPaths {
        read_path: user.clone(),
        write_path: user,
        source: ConfigSource::User,
    }
}

// ---------------------------------------------------------------------------
// config-manager.js

const MISSING_INSTANCE_URL_ERROR: &str = "Missing ServiceNow credentials. Create config/servicenow-instances.json or set SERVICENOW_INSTANCE_URL (and SERVICENOW_USERNAME / SERVICENOW_PASSWORD unless SERVICENOW_OAUTH_GRANT_TYPE=client_credentials or authorization_code) in .env";
const MISSING_INSTANCE_CREDENTIALS_ERROR: &str = "Missing ServiceNow credentials. Create config/servicenow-instances.json or set SERVICENOW_INSTANCE_URL, SERVICENOW_USERNAME, SERVICENOW_PASSWORD in .env (USERNAME and PASSWORD not required when SERVICENOW_AUTH_TYPE=oauth and SERVICENOW_OAUTH_GRANT_TYPE=client_credentials or authorization_code)";
const REDIRECT_PORT_ERROR: &str = "Invalid SERVICENOW_OAUTH_REDIRECT_PORT: expected a decimal redirect port integer from 0 to 65535.";

fn is_credential_validation_error(error: &CodedError) -> bool {
    if error.code != "INVALID_INSTANCE_CONFIG" {
        return false;
    }
    let m = error.message.to_lowercase();
    m.contains("requires clientid")
        || m.contains("requires username")
        || m.contains("requires credentialref")
        || m.contains("credential reference")
}

/// `/(?:token|secret|password|api[_-]?key|private[_-]?key)/i`
fn secret_shaped_instance_key(key: &str) -> bool {
    let k = key.to_lowercase();
    ["token", "secret", "password"]
        .iter()
        .any(|w| k.contains(w))
        || [
            "apikey",
            "api_key",
            "api-key",
            "privatekey",
            "private_key",
            "private-key",
        ]
        .iter()
        .any(|w| k.contains(w))
}

/// Strips secret-shaped keys from a raw client record, keeping the legacy
/// `password`/`clientSecret` and the non-secret `credentialRef`/`tokenUrl`.
pub fn sanitize_raw_instance(instance: &Object) -> Object {
    instance
        .iter()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "password" | "clientSecret" | "credentialRef" | "tokenUrl"
            ) || !secret_shaped_instance_key(key)
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// One row of `listInstances()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceSummary {
    pub name: String,
    pub url: String,
    pub default: bool,
    pub description: String,
}

fn str_field(o: &Object, k: &str) -> String {
    match o.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// JS truthiness of a JSON value (`undefined` = `None`).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

struct ManagerState {
    instances: Option<Arc<Vec<Object>>>,
    using_env_fallback: bool,
    /// `SetPrincipalDefault` selection (single user: process-wide, not persisted).
    principal_default: Option<String>,
    /// `RegistryStore` version; bumped by every successful `apply`.
    version: u64,
}

/// Single-user configuration facade over [`InstanceRegistry`] with the `.env`
/// fallback used only when no registry file exists.
pub struct ConfigManager {
    registry: Arc<InstanceRegistry>,
    env: Env,
    state: Mutex<ManagerState>,
    /// Held across version check, change and bump so `apply` is a true CAS.
    apply_lock: Mutex<()>,
}

impl ConfigManager {
    pub fn new(registry: Arc<InstanceRegistry>, env: Env) -> Self {
        Self {
            registry,
            env,
            state: Mutex::new(ManagerState {
                instances: None,
                using_env_fallback: false,
                principal_default: None,
                version: 1,
            }),
            apply_lock: Mutex::new(()),
        }
    }

    /// Process environment and the default registry (paths resolved lazily).
    pub fn from_process_env() -> Self {
        let env = Env::process();
        Self::new(Arc::new(InstanceRegistry::new(env.clone())), env)
    }

    pub fn registry(&self) -> &Arc<InstanceRegistry> {
        &self.registry
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ManagerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn using_env_fallback(&self) -> bool {
        self.state().using_env_fallback
    }

    /// Current `RegistryStore` version (the CAS token `apply` expects).
    pub fn registry_version(&self) -> u64 {
        self.state().version
    }

    pub fn load_instances(&self) -> Result<Arc<Vec<Object>>, CodedError> {
        if let Some(instances) = self.state().instances.clone() {
            return Ok(instances);
        }
        self.registry.load()?;
        if !self.registry.has_file() {
            return self.load_from_env();
        }
        let instances = Arc::new(
            self.registry
                .list_for_client()?
                .iter()
                .map(sanitize_raw_instance)
                .collect::<Vec<_>>(),
        );
        let mut state = self.state();
        state.using_env_fallback = false;
        state.instances = Some(instances.clone());
        Ok(instances)
    }

    pub fn load_from_env(&self) -> Result<Arc<Vec<Object>>, CodedError> {
        self.state().using_env_fallback = true;
        let env = &self.env;
        let is_oauth = env.get("SERVICENOW_AUTH_TYPE").as_deref() == Some("oauth");
        let grant_type = env.get("SERVICENOW_OAUTH_GRANT_TYPE");
        let passwordless = matches!(
            grant_type.as_deref(),
            Some("client_credentials" | "authorization_code")
        );
        let requires_user_pass = !(is_oauth && passwordless);
        let nonempty = |k: &str| env.get(k).filter(|v| !v.is_empty());
        let Some(url) = nonempty("SERVICENOW_INSTANCE_URL") else {
            return Err(CodedError::plain(MISSING_INSTANCE_URL_ERROR));
        };
        if requires_user_pass
            && (nonempty("SERVICENOW_USERNAME").is_none()
                || nonempty("SERVICENOW_PASSWORD").is_none())
        {
            return Err(CodedError::plain(MISSING_INSTANCE_CREDENTIALS_ERROR));
        }
        let mut instance = Object::new();
        instance.insert("name".into(), "default".into());
        instance.insert("url".into(), url.into());
        instance.insert(
            "username".into(),
            nonempty("SERVICENOW_USERNAME").unwrap_or_default().into(),
        );
        instance.insert(
            "password".into(),
            nonempty("SERVICENOW_PASSWORD").unwrap_or_default().into(),
        );
        instance.insert("default".into(), true.into());
        instance.insert("description".into(), "Loaded from .env".into());
        let put = |o: &mut Object, key: &str, var: &str| {
            if let Some(v) = env.get(var) {
                o.insert(key.into(), v.into());
            }
        };
        if is_oauth {
            instance.insert("authType".into(), "oauth".into());
            put(&mut instance, "clientId", "SERVICENOW_CLIENT_ID");
            put(&mut instance, "clientSecret", "SERVICENOW_CLIENT_SECRET");
            put(&mut instance, "scope", "SERVICENOW_OAUTH_SCOPE");
            if let Some(g) = grant_type.as_deref().filter(|g| !g.is_empty()) {
                instance.insert("grantType".into(), g.into());
            }
            if grant_type.as_deref() == Some("authorization_code") {
                put(
                    &mut instance,
                    "authorizeUrl",
                    "SERVICENOW_OAUTH_AUTHORIZE_URL",
                );
                put(&mut instance, "tokenUrl", "SERVICENOW_OAUTH_TOKEN_URL");
                if let Some(raw) = env.get("SERVICENOW_OAUTH_REDIRECT_PORT") {
                    instance.insert("redirectPort".into(), parse_env_redirect_port(&raw)?.into());
                }
                put(
                    &mut instance,
                    "callbackPath",
                    "SERVICENOW_OAUTH_CALLBACK_PATH",
                );
            }
        }
        let mut validation = instance.clone();
        if is_oauth && passwordless {
            validation.remove("username");
            validation.remove("password");
        }
        if let Err(error) = self.registry.validate(&Value::Object(validation)) {
            if is_credential_validation_error(&error) {
                return Err(CodedError::plain(MISSING_INSTANCE_CREDENTIALS_ERROR));
            }
            return Err(error);
        }
        let canonical = canonicalize_instance_url(&str_field(&instance, "url"))?;
        instance.insert("url".into(), canonical.into());
        let instances = Arc::new(vec![instance]);
        self.state().instances = Some(instances.clone());
        Ok(instances)
    }

    /// Re-reads the registry; on failure the previous cache is restored. A
    /// reload that changed the loaded document bumps the `RegistryStore`
    /// version (CR-6), so clients pinned to the old document are rebuilt
    /// instead of failing closed with `GenerationMoved`.
    pub fn reload(&self) -> Result<Arc<Vec<Object>>, CodedError> {
        let (previous, previous_fallback) = {
            let mut state = self.state();
            let saved = (state.instances.take(), state.using_env_fallback);
            state.using_env_fallback = false;
            saved
        };
        let result = (|| {
            self.registry.reload()?;
            if !self.registry.has_file() {
                return self.load_from_env();
            }
            let instances = Arc::new(
                self.registry
                    .list_for_client()?
                    .iter()
                    .map(sanitize_raw_instance)
                    .collect::<Vec<_>>(),
            );
            let mut state = self.state();
            state.using_env_fallback = false;
            state.instances = Some(instances.clone());
            Ok(instances)
        })();
        let mut state = self.state();
        match &result {
            Ok(instances) => {
                if previous.as_deref() != Some(instances.as_ref())
                    || previous_fallback != state.using_env_fallback
                {
                    state.version += 1;
                }
            }
            Err(_) => {
                state.instances = previous;
                state.using_env_fallback = previous_fallback;
            }
        }
        drop(state);
        result
    }

    pub fn get_instance(&self, name: &str) -> Result<Object, CodedError> {
        let instances = self.load_instances()?;
        if self.using_env_fallback() {
            return instances
                .iter()
                .find(|i| i.get("name").and_then(Value::as_str) == Some(name))
                .cloned()
                .ok_or_else(|| {
                    let names: Vec<String> =
                        instances.iter().map(|i| str_field(i, "name")).collect();
                    CodedError::plain(format!(
                        "Instance '{name}' not found. Available instances: {}",
                        names.join(", ")
                    ))
                });
        }
        Ok(sanitize_raw_instance(&self.registry.get_for_client(name)?))
    }

    pub fn get_default_instance(&self) -> Result<Object, CodedError> {
        let instances = self.load_instances()?;
        let instance = if self.using_env_fallback() {
            instances
                .iter()
                .find(|i| i.get("default") == Some(&Value::Bool(true)))
                .or_else(|| instances.first())
                .cloned()
        } else {
            self.registry
                .get_default_for_client()?
                .map(|i| sanitize_raw_instance(&i))
        };
        instance.ok_or_else(|| {
            CodedError::new("REGISTRY_EMPTY", "Instance registry contains no instances").with(
                "path",
                self.registry.write_path().to_string_lossy().into_owned(),
            )
        })
    }

    pub fn get_instance_or_default(&self, name: Option<&str>) -> Result<Object, CodedError> {
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            return self.get_instance(name);
        }
        if let Some(env_instance) = self
            .env
            .get("SERVICENOW_INSTANCE")
            .filter(|v| !v.is_empty())
        {
            return self.get_instance(&env_instance);
        }
        self.get_default_instance()
    }

    pub fn list_instances(&self) -> Result<Vec<InstanceSummary>, CodedError> {
        let instances = self.load_instances()?;
        let source: Vec<Object> = if self.using_env_fallback() {
            instances.to_vec()
        } else {
            self.registry.list()?
        };
        Ok(source
            .iter()
            .map(|i| InstanceSummary {
                name: str_field(i, "name"),
                url: str_field(i, "url"),
                default: truthy(i.get("default")),
                description: if truthy(i.get("description")) {
                    str_field(i, "description")
                } else {
                    String::new()
                },
            })
            .collect())
    }

    pub fn validate_instance(&self, instance: &Value) -> Result<(), CodedError> {
        self.registry.validate(instance)
    }
}

// ---------------------------------------------------------------------------
// RegistryStore (single user): instance id = instance name.

fn registry_error(e: CodedError) -> RegistryError {
    if e.code.starts_with("INVALID_") || e.code.starts_with("INSTANCE_") {
        RegistryError::Invalid(e.message)
    } else {
        RegistryError::Storage(Box::new(std::io::Error::other(e.code)))
    }
}

/// Non-secret auth metadata of an instance record.
fn auth_metadata(instance: &Object) -> Value {
    let options = instance_to_client_options(instance);
    let keep = [
        "authType",
        "clientId",
        "grantType",
        "scope",
        "authorizeUrl",
        "tokenUrl",
        "redirectPort",
        "callbackPath",
        "credentialRef",
    ];
    Value::Object(
        options
            .into_iter()
            .filter(|(k, _)| keep.contains(&k.as_str()))
            .collect(),
    )
}

impl ConfigManager {
    fn view(&self) -> RegistryView {
        let (version, principal_default) = {
            let s = self.state();
            (s.version, s.principal_default.clone())
        };
        let instances = self
            .load_instances()
            .map(|i| i.to_vec())
            .unwrap_or_default();
        let entries: Vec<InstanceEntry> = instances
            .iter()
            .map(|i| InstanceEntry {
                instance_id: str_field(i, "name"),
                name: str_field(i, "name"),
                canonical_url: str_field(i, "url"),
                auth: auth_metadata(i),
                config_version: version,
            })
            .collect();
        let env_default = self
            .env
            .get("SERVICENOW_INSTANCE")
            .filter(|v| !v.is_empty());
        let (selected_default, default_source) = if let Some(p) = principal_default {
            (Some(p), DefaultSource::Principal)
        } else if let Some(e) = env_default {
            (Some(e), DefaultSource::Env)
        } else if let Ok(d) = self.get_default_instance() {
            (Some(str_field(&d, "name")), DefaultSource::Registry)
        } else {
            (None, DefaultSource::None)
        };
        RegistryView {
            registry_version: version,
            instances: entries,
            selected_default,
            default_source,
            env_fallback: self.using_env_fallback(),
        }
    }

    fn apply_change(&self, change: RegistryChange) -> Result<(), RegistryError> {
        let exists = |id: &str| {
            self.load_instances()
                .map_err(registry_error)?
                .iter()
                .any(|i| i.get("name").and_then(Value::as_str) == Some(id))
                .then_some(())
                .ok_or_else(|| RegistryError::Invalid(format!("Instance '{id}' not found")))
        };
        let writable = || {
            if self.using_env_fallback() {
                Err(RegistryError::ReadOnly)
            } else {
                Ok(())
            }
        };
        match change {
            RegistryChange::SetPrincipalDefault { instance_id, .. } => {
                exists(&instance_id)?;
                self.state().principal_default = Some(instance_id);
                return Ok(());
            }
            RegistryChange::SetRegistryDefault { instance_id } => {
                self.load_instances().map_err(registry_error)?;
                writable()?;
                let mut patch = Object::new();
                patch.insert("default".into(), true.into());
                self.registry
                    .update(&instance_id, &patch)
                    .map_err(registry_error)?;
            }
            RegistryChange::Remove { instance_id } => {
                self.load_instances().map_err(registry_error)?;
                writable()?;
                self.registry
                    .remove(&instance_id, None)
                    .map_err(registry_error)?;
                let mut s = self.state();
                if s.principal_default.as_deref() == Some(instance_id.as_str()) {
                    s.principal_default = None;
                }
            }
            RegistryChange::Upsert(entry) => {
                self.load_instances().map_err(registry_error)?;
                writable()?;
                let mut record = match entry.auth {
                    Value::Object(o) => o,
                    Value::Null => Object::new(),
                    _ => return Err(RegistryError::Invalid("auth must be an object".into())),
                };
                record.insert("url".into(), entry.canonical_url.into());
                let found = self.registry.get(&entry.name).is_ok();
                if found {
                    self.registry
                        .update(&entry.name, &record)
                        .map_err(registry_error)?;
                } else {
                    record.insert("name".into(), entry.name.into());
                    self.registry
                        .register(&Value::Object(record), RegisterOptions::default())
                        .map_err(registry_error)?;
                }
            }
            _ => return Err(RegistryError::Invalid("unsupported registry change".into())),
        }
        self.reload().map_err(registry_error)?;
        Ok(())
    }
}

impl RegistryStore for ConfigManager {
    fn snapshot_for(&self, _principal: &Principal) -> Arc<RegistryView> {
        Arc::new(self.view())
    }

    fn apply<'a>(
        &'a self,
        expected_version: u64,
        change: RegistryChange,
    ) -> BoxFut<'a, Result<u64, RegistryError>> {
        Box::pin(async move { self.apply_cas(expected_version, change) })
    }
}

impl ConfigManager {
    /// Compare-and-swap: check, change and bump under one lock (synchronous body,
    /// so no guard is held across an await).
    fn apply_cas(
        &self,
        expected_version: u64,
        change: RegistryChange,
    ) -> Result<u64, RegistryError> {
        let _cas = self
            .apply_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let current = self.state().version;
        if current != expected_version {
            return Err(RegistryError::Conflict {
                expected: expected_version,
                current,
            });
        }
        self.apply_change(change)?;
        // The reload inside `apply_change` already bumped when the document
        // changed (CR-6); an apply moves the version by exactly one either way.
        let mut s = self.state();
        s.version = s.version.max(expected_version + 1);
        Ok(s.version)
    }
}

fn parse_env_redirect_port(raw: &str) -> Result<u64, CodedError> {
    let trimmed = js_trim(raw);
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CodedError::plain(REDIRECT_PORT_ERROR));
    }
    match trimmed.parse::<u64>() {
        Ok(port) if port <= 65535 => Ok(port),
        _ => Err(CodedError::plain(REDIRECT_PORT_ERROR)),
    }
}

/// `String.prototype.trim` (Unicode whitespace and line terminators).
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `instanceToClientOptions`: the auth fields every client construction forwards.
pub fn instance_to_client_options(instance: &Object) -> Object {
    let mut out = Object::new();
    out.insert(
        "authType".into(),
        instance
            .get("authType")
            .filter(|v| truthy(Some(v)))
            .cloned()
            .unwrap_or_else(|| "basic".into()),
    );
    for key in [
        "clientId",
        "clientSecret",
        "password",
        "credentialRef",
        "grantType",
        "scope",
        "authorizeUrl",
        "tokenUrl",
        "redirectPort",
        "callbackPath",
    ] {
        if let Some(v) = instance.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    out
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
