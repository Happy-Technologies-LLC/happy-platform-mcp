//! `happy-platform-mcp instance …` (port of `src/instance-cli.js`). Secrets are
//! accepted only through masked prompts; registry errors print only their code
//! and safe details.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hpm_core::config::{CodedError, Env, Object, resolve_path};
use hpm_core::keychain::{
    CredentialType, InstanceCredentialStore, SecretStore, credential_ref_for, instance_origin,
};
use hpm_core::registry::{
    InstanceRegistry, RegisterOptions, is_secret_shaped_key, redact_secrets,
    resolve_oauth_endpoints,
};
use hpm_core::token_store::{
    FileTokenStore, KeychainTokenStore, TokenStore, clear_instance_refresh_tokens,
    default_token_dir, oauth_refresh_token_account,
};
use hpm_types::BoxFut;
use serde_json::{Value, json};
use zeroize::Zeroizing;

pub const SECRET_ARGUMENT_ERROR: &str =
    "Secret flags and values are not accepted in command arguments; use a masked prompt.";
/// First line of `instance migrate` before its confirm prompt (not in 6.2.0).
pub const MIGRATION_SOURCE_PREFIX: &str = "Migration source: ";
const NON_INTERACTIVE: &str = "Non-interactive use requires an interactive TTY for masked prompts; rerun in a terminal or inject the required prompt function.";
/// 6.2.0 text, verbatim (CLI goldens). `instance credential bind <name>` is
/// not listed; the unbound-secret errors print that command instead.
pub const USAGE: &str = "Usage:
  happy-platform-mcp instance add
  happy-platform-mcp instance list
  happy-platform-mcp instance update <name> [metadata flags; auth changes require remove/re-add]
  happy-platform-mcp instance test <name>
  happy-platform-mcp instance remove <name>
  happy-platform-mcp instance credential set <name> --type password|client-secret
  happy-platform-mcp instance migrate";

const METADATA_FLAGS: [(&str, &str); 14] = [
    ("name", "name"),
    ("url", "url"),
    ("auth-type", "authType"),
    ("grant-type", "grantType"),
    ("username", "username"),
    ("client-id", "clientId"),
    ("scope", "scope"),
    ("authorize-url", "authorizeUrl"),
    ("token-url", "tokenUrl"),
    ("redirect-port", "redirectPort"),
    ("callback-path", "callbackPath"),
    ("description", "description"),
    ("default", "default"),
    ("no-default", "noDefault"),
];

// ---------------------------------------------------------------------------
// Prompts

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PromptKind {
    Input,
    Password,
    Select,
    Confirm,
}

/// One prompt request (`@inquirer/prompts` options).
#[derive(Clone, Debug)]
pub struct PromptSpec {
    pub name: String,
    pub message: String,
    pub default: Option<Value>,
    pub mask: bool,
    /// `(label, value)` for `select`.
    pub choices: Vec<(String, String)>,
}

/// Answers prompts. `None` = no answer (the prompt default applies).
pub trait Prompter: Send {
    /// Kinds this prompter answers itself (JS "injected" prompt functions).
    fn injected(&self) -> BTreeSet<PromptKind>;
    fn ask(&mut self, kind: PromptKind, spec: &PromptSpec) -> Result<Option<Value>, CodedError>;
}

/// Terminal prompts (`inquire`); used only when stdin and stdout are TTYs.
pub struct TerminalPrompter;

fn prompt_failed(e: impl std::fmt::Display) -> CodedError {
    CodedError::plain(e.to_string())
}

impl Prompter for TerminalPrompter {
    fn injected(&self) -> BTreeSet<PromptKind> {
        BTreeSet::new()
    }
    fn ask(&mut self, kind: PromptKind, spec: &PromptSpec) -> Result<Option<Value>, CodedError> {
        let default_str = spec
            .default
            .as_ref()
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(Some(match kind {
            PromptKind::Input => {
                let mut p = inquire::Text::new(&spec.message);
                if let Some(d) = default_str.as_deref() {
                    p = p.with_default(d);
                }
                Value::String(p.prompt().map_err(prompt_failed)?)
            }
            PromptKind::Password => Value::String(
                inquire::Password::new(&spec.message)
                    .with_display_mode(inquire::PasswordDisplayMode::Masked)
                    .without_confirmation()
                    .with_validator(|v: &str| {
                        Ok(if v.is_empty() {
                            inquire::validator::Validation::Invalid("A value is required.".into())
                        } else {
                            inquire::validator::Validation::Valid
                        })
                    })
                    .prompt()
                    .map_err(prompt_failed)?,
            ),
            PromptKind::Select => {
                let labels: Vec<String> = spec.choices.iter().map(|(l, _)| l.clone()).collect();
                let start = default_str
                    .as_deref()
                    .and_then(|d| spec.choices.iter().position(|(_, v)| v == d))
                    .unwrap_or(0);
                let chosen = inquire::Select::new(&spec.message, labels)
                    .with_starting_cursor(start)
                    .prompt()
                    .map_err(prompt_failed)?;
                let value = spec
                    .choices
                    .iter()
                    .find(|(l, _)| *l == chosen)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                Value::String(value)
            }
            PromptKind::Confirm => Value::Bool(
                inquire::Confirm::new(&spec.message)
                    .with_default(
                        spec.default
                            .as_ref()
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                    .prompt()
                    .map_err(prompt_failed)?,
            ),
        }))
    }
}

// ---------------------------------------------------------------------------
// Connectivity probe

/// Failure of the `instance test` read.
#[derive(Debug)]
pub struct ProbeError {
    pub status: Option<u16>,
    pub message: String,
}

/// The authenticated read behind `instance test`.
pub trait Probe: Send + Sync {
    fn probe<'a>(
        &'a self,
        instance: &'a Object,
        credentials: &'a InstanceCredentialStore,
    ) -> BoxFut<'a, Result<(), ProbeError>>;
}

/// `GET /api/now/table/sys_user?sysparm_limit=1` with the instance's credentials.
pub struct HttpProbe;

fn probe_error(message: impl Into<String>) -> ProbeError {
    ProbeError {
        status: None,
        message: message.into(),
    }
}

fn credential_probe_error(e: CodedError) -> ProbeError {
    if e.code == "CREDENTIAL_NOT_FOUND" {
        probe_error("Credential not found")
    } else {
        probe_error(if e.message.is_empty() {
            e.code
        } else {
            e.message
        })
    }
}

fn str_of<'a>(o: &'a Object, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str)
}

fn credential_ref(instance: &Object, kind: &str) -> Option<String> {
    match instance.get("credentialRef") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(o)) => o.get(kind).and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// Connect / overall deadlines of the probe (6.2.0 axios uses a 30 s timeout).
const PROBE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Redirects are never followed (6.2.0 `postTokenForm`: `maxRedirects: 0`): a 3xx
/// must not carry the client secret, password or bearer token elsewhere. A 3xx is
/// reported as the non-success status it is.
fn probe_client() -> Result<reqwest::Client, ProbeError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(PROBE_CONNECT_TIMEOUT)
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| probe_error(e.to_string()))
}

impl Probe for HttpProbe {
    fn probe<'a>(
        &'a self,
        instance: &'a Object,
        credentials: &'a InstanceCredentialStore,
    ) -> BoxFut<'a, Result<(), ProbeError>> {
        Box::pin(async move {
            let base = str_of(instance, "url").unwrap_or("").to_owned();
            // Owner rule, as in the server: only a secret bound to this
            // origin is read; an unbound or moved one is refused unsent.
            let origin = instance_origin(&base).map_err(credential_probe_error)?;
            let http = probe_client()?;
            let target = format!("{base}/api/now/table/sys_user?sysparm_limit=1");
            let username = str_of(instance, "username").unwrap_or("").to_owned();
            let request = if str_of(instance, "authType").unwrap_or("basic") == "oauth" {
                let grant = effective_grant_type(instance);
                if grant == "authorization_code" {
                    return Err(probe_error(
                        "Authorization-code instances require a browser sign-in through the MCP server",
                    ));
                }
                let reference = credential_ref(instance, "clientSecret")
                    .ok_or_else(|| probe_error("Credential not found"))?;
                let secret = credentials
                    .get_secret_for_origin(&reference, &origin)
                    .await
                    .map_err(credential_probe_error)?;
                let client_id = str_of(instance, "clientId").unwrap_or("").to_owned();
                // Zeroized on drop; the encoded body handed to reqwest is not.
                let mut pairs: Vec<(&str, Zeroizing<String>)> = vec![
                    ("grant_type", Zeroizing::new(grant.to_owned())),
                    ("client_id", Zeroizing::new(client_id)),
                    ("client_secret", Zeroizing::new(secret.expose().to_owned())),
                ];
                if grant == "password" {
                    let reference = credential_ref(instance, "password")
                        .ok_or_else(|| probe_error("Credential not found"))?;
                    let password = credentials
                        .get_secret_for_origin(&reference, &origin)
                        .await
                        .map_err(credential_probe_error)?;
                    pairs.push(("username", Zeroizing::new(username)));
                    pairs.push(("password", Zeroizing::new(password.expose().to_owned())));
                }
                if let Some(scope) = str_of(instance, "scope").filter(|s| !s.is_empty()) {
                    pairs.push(("scope", Zeroizing::new(scope.to_owned())));
                }
                let (_, token_url) =
                    resolve_oauth_endpoints(&base, None, str_of(instance, "tokenUrl"));
                let borrowed: Vec<(&str, &str)> =
                    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let response = http
                    .post(&token_url)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(form(&borrowed))
                    .send()
                    .await
                    .map_err(|e| probe_error(e.to_string()))?;
                let status = response.status();
                if !status.is_success() {
                    return Err(ProbeError {
                        status: Some(status.as_u16()),
                        message: format!(
                            "OAuth token request failed with status {}",
                            status.as_u16()
                        ),
                    });
                }
                let body: Value = response
                    .json()
                    .await
                    .map_err(|e| probe_error(e.to_string()))?;
                let token = body
                    .get("access_token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        probe_error("OAuth token response did not include access_token")
                    })?
                    .to_owned();
                let token = Zeroizing::new(token);
                http.get(&target).bearer_auth(token.as_str())
            } else {
                let reference = credential_ref(instance, "password")
                    .ok_or_else(|| probe_error("Credential not found"))?;
                let password = credentials
                    .get_secret_for_origin(&reference, &origin)
                    .await
                    .map_err(credential_probe_error)?;
                http.get(&target)
                    .basic_auth(username, Some(password.expose()))
            };
            let response = request
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|e| probe_error(e.to_string()))?;
            let status = response.status();
            if status.is_success() {
                Ok(())
            } else {
                Err(ProbeError {
                    status: Some(status.as_u16()),
                    message: format!("Request failed with status code {}", status.as_u16()),
                })
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Registry seam

/// The registry operations the CLI uses (`InstanceRegistry`; tests inject fakes).
pub trait CliRegistry: Send + Sync {
    fn list(&self) -> Result<Vec<Object>, CodedError>;
    fn get(&self, name: &str) -> Result<Object, CodedError>;
    fn validate(&self, instance: &Value) -> Result<(), CodedError>;
    fn validate_document(&self, document: &Value) -> Result<Value, CodedError>;
    fn register(&self, instance: &Value, make_default: bool) -> Result<Object, CodedError>;
    fn update(&self, name: &str, patch: &Object) -> Result<Object, CodedError>;
    fn remove(&self, name: &str) -> Result<(), CodedError>;
    fn raw_document(&self) -> Result<Value, CodedError>;
    fn read_path(&self) -> PathBuf;
    fn write_path(&self) -> PathBuf;
    fn write_migrated_document(&self, document: &Value) -> Result<(), CodedError>;
}

impl CliRegistry for InstanceRegistry {
    fn list(&self) -> Result<Vec<Object>, CodedError> {
        InstanceRegistry::list(self)
    }
    fn get(&self, name: &str) -> Result<Object, CodedError> {
        InstanceRegistry::get(self, name)
    }
    fn validate(&self, instance: &Value) -> Result<(), CodedError> {
        InstanceRegistry::validate(self, instance)
    }
    fn validate_document(&self, document: &Value) -> Result<Value, CodedError> {
        InstanceRegistry::validate_document(self, document)
    }
    fn register(&self, instance: &Value, make_default: bool) -> Result<Object, CodedError> {
        InstanceRegistry::register(
            self,
            instance,
            RegisterOptions {
                make_default,
                ..Default::default()
            },
        )
    }
    fn update(&self, name: &str, patch: &Object) -> Result<Object, CodedError> {
        InstanceRegistry::update(self, name, patch)
    }
    fn remove(&self, name: &str) -> Result<(), CodedError> {
        InstanceRegistry::remove(self, name, None)
    }
    fn raw_document(&self) -> Result<Value, CodedError> {
        InstanceRegistry::raw_document(self)
    }
    fn read_path(&self) -> PathBuf {
        InstanceRegistry::read_path(self)
    }
    fn write_path(&self) -> PathBuf {
        InstanceRegistry::write_path(self)
    }
    fn write_migrated_document(&self, document: &Value) -> Result<(), CodedError> {
        InstanceRegistry::write_migrated_document(self, document)
    }
}

// ---------------------------------------------------------------------------
// Context

/// Everything a command touches; tests inject fakes.
pub struct CliContext<'a> {
    pub registry: Arc<dyn CliRegistry>,
    pub credentials: InstanceCredentialStore,
    pub prompter: Box<dyn Prompter>,
    /// stdin and stdout are both terminals.
    pub tty: bool,
    pub probe: Box<dyn Probe>,
    /// OS keychain and file refresh-token stores (cleanup covers both).
    pub token_stores: Vec<(&'static str, Arc<dyn TokenStore>)>,
    pub os_user: String,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
}

impl<'a> CliContext<'a> {
    /// Production wiring from the environment.
    pub fn from_env(
        env: &Env,
        secrets: Arc<dyn SecretStore>,
        stdout: &'a mut dyn Write,
        stderr: &'a mut dyn Write,
    ) -> Self {
        use std::io::IsTerminal;
        let mut token_stores: Vec<(&'static str, Arc<dyn TokenStore>)> = vec![(
            "OS keychain",
            Arc::new(KeychainTokenStore::new(secrets.clone())),
        )];
        let dir = default_token_dir(env);
        if let Ok(file) = FileTokenStore::new(&dir)
            && (!cfg!(windows) || dir.exists())
        {
            token_stores.push(("file token store", Arc::new(file)));
        }
        Self {
            registry: Arc::new(InstanceRegistry::new(env.clone())),
            credentials: InstanceCredentialStore::new(secrets),
            prompter: Box::new(TerminalPrompter),
            tty: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            probe: Box::new(HttpProbe),
            token_stores,
            os_user: hpm_core::token_store::os_user().unwrap_or_default(),
            stdout,
            stderr,
        }
    }

    fn out(&mut self, line: &str) {
        let _ = writeln!(self.stdout, "{line}");
    }
    fn err(&mut self, line: &str) {
        let _ = writeln!(self.stderr, "{line}");
    }

    fn ask(&mut self, kind: PromptKind, spec: PromptSpec) -> Result<Value, CodedError> {
        if !self.tty && !self.prompter.injected().contains(&kind) {
            return Err(CodedError::new("NON_INTERACTIVE_PROMPT", NON_INTERACTIVE));
        }
        let answer = self.prompter.ask(kind, &spec)?;
        Ok(match answer {
            None | Some(Value::Null) => spec.default.unwrap_or(Value::Null),
            Some(v) => v,
        })
    }
}

fn spec(name: &str, message: &str, default: Option<Value>) -> PromptSpec {
    PromptSpec {
        name: name.into(),
        message: message.into(),
        default,
        mask: false,
        choices: Vec::new(),
    }
}

fn select_spec(
    name: &str,
    message: &str,
    default: Option<&str>,
    choices: &[(&str, &str)],
) -> PromptSpec {
    PromptSpec {
        choices: choices
            .iter()
            .map(|(l, v)| ((*l).to_owned(), (*v).to_owned()))
            .collect(),
        ..spec(name, message, default.map(Value::from))
    }
}

// ---------------------------------------------------------------------------
// Helpers

fn redact(value: &Value, inside_credential_ref: bool) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| redact(i, inside_credential_ref))
                .collect(),
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| {
                    if inside_credential_ref {
                        k.as_str() == "password" || k.as_str() == "clientSecret"
                    } else {
                        !is_secret_shaped_key(k)
                    }
                })
                .map(|(k, v)| (k.clone(), redact(v, k == "credentialRef")))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// 6.2.0 `JSON.stringify(stable(value), null, 2)`: object keys sorted recursively, 2-space indent.
/// Sorting is explicit because the workspace enables serde_json `preserve_order`.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(&stable(value)).unwrap_or_default()
}

fn stable(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(stable).collect()),
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            // JS `Array.prototype.sort()` compares UTF-16 code units.
            entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), stable(v)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

fn message_for_error(error: &CodedError) -> String {
    if !error.message.is_empty() {
        error.message.clone()
    } else if !error.code.is_empty() {
        error.code.clone()
    } else {
        "Operation failed".into()
    }
}

fn is_field_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn safe_registry_error(error: &CodedError) -> String {
    let code = if error.code.len() > "REGISTRY_".len()
        && error.code.starts_with("REGISTRY_")
        && error
            .code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        error.code.clone()
    } else {
        "REGISTRY_OPERATION_FAILED".into()
    };
    let mut fields = Vec::new();
    if let Some(p) = error.detail_str("path").filter(|p| !p.is_empty()) {
        fields.push(format!("path={}", resolve_path(Path::new(p)).display()));
    }
    if code != "REGISTRY_WRITE_FAILED"
        && let Some(f) = error.detail_str("field").filter(|f| is_field_name(f))
    {
        fields.push(format!("field={f}"));
    }
    if fields.is_empty() {
        code
    } else {
        format!("{code} ({})", fields.join(", "))
    }
}

fn safe_migration_error(error: &CodedError) -> String {
    let fields: Vec<String> = ["readPath", "writePath"]
        .iter()
        .filter_map(|f| {
            error
                .detail_str(f)
                .filter(|p| !p.is_empty())
                .map(|p| format!("{f}={}", resolve_path(Path::new(p)).display()))
        })
        .collect();
    let suffix = if fields.is_empty() {
        String::new()
    } else {
        format!(" ({})", fields.join(", "))
    };
    format!("{}{suffix}: {}", error.code, error.message)
}

fn cli_error_message(error: &CodedError) -> String {
    if error.code.starts_with("REGISTRY_") {
        safe_registry_error(error)
    } else {
        message_for_error(error)
    }
}

fn normalized_argument_name(value: &str) -> String {
    let v = value.strip_prefix('-').unwrap_or(value);
    let v = v.strip_prefix('-').unwrap_or(v);
    v.chars()
        .filter(|c| *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_secret_name(name: &str) -> bool {
    name == "password" || name == "clientsecret"
}

fn assignment_name(token: &str) -> Option<String> {
    match token.find('=') {
        Some(i) if i > 0 => Some(normalized_argument_name(&token[..i])),
        _ => None,
    }
}

fn is_secret_flag(token: &str) -> bool {
    if !token.starts_with('-') {
        return false;
    }
    let name = match token.find('=') {
        Some(i) => &token[..i],
        None => token,
    };
    is_secret_name(&normalized_argument_name(name))
}

fn type_value(tokens: &[String], index: usize) -> Option<Option<&str>> {
    let token = &tokens[index];
    match token.find('=') {
        Some(i) if normalized_argument_name(&token[..i]) == "type" => Some(Some(&token[i + 1..])),
        None if normalized_argument_name(token) == "type" => {
            Some(tokens.get(index + 1).map(String::as_str))
        }
        _ => None,
    }
}

/// True when any argument names or carries a secret (checked before dispatch).
pub fn contains_secret_argument(tokens: &[String]) -> bool {
    (0..tokens.len()).any(|index| {
        let token = &tokens[index];
        if is_secret_flag(token) || assignment_name(token).is_some_and(|n| is_secret_name(&n)) {
            return true;
        }
        match type_value(tokens, index) {
            // JS `String(undefined)` is "undefined": not a safe type.
            Some(requested) => !matches!(requested, Some("password" | "client-secret")),
            None => false,
        }
    })
}

fn usage_error(message: impl Into<String>) -> CodedError {
    CodedError::new("USAGE", message)
}

/// Parsed `--flag` values (`Value::Bool(true)` for bare boolean flags).
type Flags = std::collections::BTreeMap<String, Value>;

fn parse_flags(
    tokens: &[String],
    allowed: &[(&str, bool)],
) -> Result<(Flags, Vec<String>), CodedError> {
    let mut flags = Flags::new();
    let mut positionals = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if !token.starts_with("--") {
            positionals.push(token.clone());
            index += 1;
            continue;
        }
        let equals = token.find('=');
        let raw = match equals {
            Some(i) => &token[2..i],
            None => &token[2..],
        };
        let name = raw.to_lowercase();
        if is_secret_flag(token) || assignment_name(token).is_some_and(|n| is_secret_name(&n)) {
            return Err(usage_error(SECRET_ARGUMENT_ERROR));
        }
        let Some((_, boolean)) = allowed.iter().find(|(n, _)| *n == name) else {
            return Err(usage_error("Unknown option."));
        };
        let next_is_flag = index + 1 >= tokens.len() || tokens[index + 1].starts_with("--");
        if *boolean
            || ((name == "no-default" || name == "default") && equals.is_none() && next_is_flag)
        {
            flags.insert(name, Value::Bool(true));
        } else if let Some(i) = equals {
            flags.insert(name, Value::String(token[i + 1..].to_owned()));
        } else if next_is_flag {
            return Err(usage_error("Option requires a value."));
        } else {
            index += 1;
            flags.insert(name, Value::String(tokens[index].clone()));
        }
        index += 1;
    }
    Ok((flags, positionals))
}

fn parse_boolean(value: &Value, field: &str) -> Result<bool, CodedError> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s == "true" => Ok(true),
        Value::String(s) if s == "false" => Ok(false),
        _ => Err(usage_error(format!(
            "Option --{field} must be true or false."
        ))),
    }
}

fn parse_redirect_port(value: &Value) -> Result<Value, CodedError> {
    let raw = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let trimmed = hpm_core::config::js_trim(&raw);
    let error = || usage_error("Option --redirect-port must be a decimal integer from 0 to 65535.");
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(error());
    }
    match trimmed.parse::<u64>() {
        Ok(p) if p <= 65535 => Ok(Value::from(p)),
        _ => Err(error()),
    }
}

fn effective_grant_type(instance: &Object) -> &str {
    match instance.get("grantType") {
        None => {
            if hpm_core::config::truthy(instance.get("username")) {
                "password"
            } else {
                "client_credentials"
            }
        }
        Some(Value::String(s)) => s,
        Some(_) => "",
    }
}

fn persists_refresh_tokens(instance: &Object) -> bool {
    str_of(instance, "authType") == Some("oauth")
        && effective_grant_type(instance) == "authorization_code"
}

fn instance_credential_refs(instance: &Object) -> Vec<String> {
    match instance.get("credentialRef") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Object(o)) => o
            .values()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn expected_credential_type(instance: &Object, requested: &str) -> Option<CredentialType> {
    let kind = CredentialType::parse(requested)?;
    match str_of(instance, "authType") {
        Some("basic") => (kind == CredentialType::Password).then_some(kind),
        Some("oauth") => match effective_grant_type(instance) {
            "authorization_code" => None,
            "password" => Some(kind),
            _ => (kind == CredentialType::ClientSecret).then_some(kind),
        },
        _ => None,
    }
}

fn credential_ref_patch(instance: &Object, kind: CredentialType, reference: &str) -> Object {
    let mut patch = Object::new();
    if str_of(instance, "authType") == Some("oauth") && effective_grant_type(instance) == "password"
    {
        let mut current = match instance.get("credentialRef") {
            Some(Value::Object(o)) => o.clone(),
            _ => Object::new(),
        };
        let key = if kind == CredentialType::Password {
            "password"
        } else {
            "clientSecret"
        };
        current.insert(key.into(), reference.into());
        patch.insert("credentialRef".into(), Value::Object(current));
    } else {
        patch.insert("credentialRef".into(), reference.into());
    }
    patch
}

fn merged(base: &Object, patch: &Object) -> Object {
    let mut out = base.clone();
    for (k, v) in patch {
        out.insert(k.clone(), v.clone());
    }
    out
}

fn to_ref(kind: CredentialType, name: &str) -> Result<String, CodedError> {
    credential_ref_for(name, kind)
}

// ---------------------------------------------------------------------------
// Credential snapshots

struct Snapshot {
    reference: String,
    existed: bool,
    value: Option<Zeroizing<String>>,
    /// The origin the secret was bound to; restored with it.
    binding: Option<String>,
}

async fn has_secret(store: &InstanceCredentialStore, reference: &str) -> Result<bool, CodedError> {
    match store.has_secret(reference).await {
        Err(e) if e.code == "CREDENTIAL_NOT_FOUND" => Ok(false),
        other => other,
    }
}

async fn snapshot_secret(
    store: &InstanceCredentialStore,
    reference: &str,
) -> Result<Snapshot, CodedError> {
    if !has_secret(store, reference).await? {
        return Ok(Snapshot {
            reference: reference.into(),
            existed: false,
            value: None,
            binding: None,
        });
    }
    let value = Zeroizing::new(store.get_secret(reference).await?.expose().to_owned());
    let binding = store.bound_origin(reference).await?;
    Ok(Snapshot {
        reference: reference.into(),
        existed: true,
        value: Some(value),
        binding,
    })
}

async fn set_with_snapshot(
    store: &InstanceCredentialStore,
    reference: &str,
    value: &str,
    origin: &str,
    snapshots: &mut Vec<Snapshot>,
) -> Result<(), CodedError> {
    if !snapshots.iter().any(|s| s.reference == reference) {
        snapshots.push(snapshot_secret(store, reference).await?);
    }
    store.set_secret(reference, value, origin).await
}

/// Origin of the instance URL registered (or being registered) under a name:
/// every saved secret is bound to it at write time.
fn origin_of_instance(instance: &Object) -> Result<String, CodedError> {
    instance_origin(str_of(instance, "url").unwrap_or(""))
}

async fn restore_snapshots(
    ctx: &mut CliContext<'_>,
    snapshots: &[Snapshot],
    label: &str,
) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    for snapshot in snapshots.iter().rev() {
        let ok = if snapshot.existed {
            ctx.credentials
                .restore_secret(
                    &snapshot.reference,
                    snapshot.value.as_ref().map_or("", |v| v.as_str()),
                    snapshot.binding.as_deref(),
                )
                .await
                .is_ok()
        } else {
            matches!(
                ctx.credentials.delete_secret(&snapshot.reference).await,
                Ok(true)
            )
        };
        if !ok {
            failures.push(snapshot.reference.clone());
        }
    }
    if !failures.is_empty() {
        let mut unique: Vec<String> = Vec::new();
        for f in &failures {
            if !unique.contains(f) {
                unique.push(f.clone());
            }
        }
        ctx.err(&format!(
            "{label} failed for {}. Verify registry metadata and keychain entries before retrying.",
            unique.join(", ")
        ));
    }
    failures
}

async fn clear_refresh_tokens_everywhere(
    ctx: &mut CliContext<'_>,
    instance: &Object,
) -> Result<(), CodedError> {
    for (_, store) in &ctx.token_stores {
        clear_instance_refresh_tokens(store.as_ref(), instance, &ctx.os_user).await?;
    }
    let labels: Vec<&str> = ctx.token_stores.iter().map(|(l, _)| *l).collect();
    let line = format!(
        "Cleared OAuth refresh tokens for '{}' from: {}.",
        str_of(instance, "name").unwrap_or(""),
        labels.join(", ")
    );
    ctx.out(&line);
    Ok(())
}

async fn clear_changed_oauth_identity(
    ctx: &mut CliContext<'_>,
    existing: &Object,
    patch: &Object,
) -> Result<(), CodedError> {
    if !persists_refresh_tokens(existing) {
        return Ok(());
    }
    let candidate = merged(existing, patch);
    ctx.registry.validate(&Value::Object(candidate.clone()))?;
    let changed = match (
        oauth_refresh_token_account(&candidate, &ctx.os_user),
        oauth_refresh_token_account(existing, &ctx.os_user),
    ) {
        (Ok(a), Ok(b)) => a != b,
        _ => return Ok(()),
    };
    if changed {
        clear_refresh_tokens_everywhere(ctx, existing).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands

async fn prompt_metadata(ctx: &mut CliContext<'_>, initial: &Object) -> Result<Object, CodedError> {
    let mut result = Object::new();
    let take = |ctx: &mut CliContext<'_>,
                field: &str,
                s: PromptSpec,
                kind: PromptKind|
     -> Result<Value, CodedError> {
        match initial.get(field) {
            Some(v) => Ok(v.clone()),
            None => ctx.ask(kind, s),
        }
    };
    let name = take(
        ctx,
        "name",
        spec("name", "Instance name:", None),
        PromptKind::Input,
    )?;
    result.insert("name".into(), name);
    let url = take(
        ctx,
        "url",
        spec("url", "ServiceNow URL:", None),
        PromptKind::Input,
    )?;
    result.insert("url".into(), url);
    let auth = take(
        ctx,
        "authType",
        select_spec(
            "authType",
            "Authentication type:",
            Some("basic"),
            &[("Basic authentication", "basic"), ("OAuth", "oauth")],
        ),
        PromptKind::Select,
    )?;
    let is_basic = auth == json!("basic");
    let is_oauth = auth == json!("oauth");
    result.insert("authType".into(), auth);
    if is_basic {
        let u = take(
            ctx,
            "username",
            spec("username", "Username:", None),
            PromptKind::Input,
        )?;
        result.insert("username".into(), u);
    } else {
        let grant = take(
            ctx,
            "grantType",
            select_spec(
                "grantType",
                "OAuth grant type:",
                Some("client_credentials"),
                &[
                    ("Client credentials", "client_credentials"),
                    ("Password grant", "password"),
                    ("Authorization code", "authorization_code"),
                ],
            ),
            PromptKind::Select,
        )?;
        let is_password = grant == json!("password");
        result.insert("grantType".into(), grant);
        let cid = take(
            ctx,
            "clientId",
            spec("clientId", "Client ID:", None),
            PromptKind::Input,
        )?;
        result.insert("clientId".into(), cid);
        if is_password {
            let u = take(
                ctx,
                "username",
                spec("username", "Username:", None),
                PromptKind::Input,
            )?;
            result.insert("username".into(), u);
        }
    }
    for (name, message) in [
        ("scope", "OAuth scope:"),
        ("authorizeUrl", "Authorization URL:"),
        ("tokenUrl", "Token URL:"),
        ("callbackPath", "Callback path:"),
        ("description", "Description:"),
    ] {
        if let Some(v) = initial.get(name) {
            result.insert(name.into(), v.clone());
        } else if is_oauth || name == "description" {
            let value = ctx.ask(PromptKind::Input, spec(name, message, Some(json!(""))))?;
            if hpm_core::config::truthy(Some(&value)) {
                result.insert(name.into(), value);
            }
        }
    }
    for k in ["redirectPort", "default"] {
        if let Some(v) = initial.get(k) {
            result.insert(k.into(), v.clone());
        }
    }
    Ok(result)
}

/// The masked answer, zeroized on drop.
fn prompt_secret(
    ctx: &mut CliContext<'_>,
    name: &str,
    message: &str,
) -> Result<Zeroizing<String>, CodedError> {
    let value = ctx.ask(
        PromptKind::Password,
        PromptSpec {
            mask: true,
            ..spec(name, message, None)
        },
    )?;
    Ok(Zeroizing::new(match value {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    }))
}

fn all_metadata_flags() -> Vec<(&'static str, bool)> {
    METADATA_FLAGS.iter().map(|(f, _)| (*f, false)).collect()
}

async fn add_command(args: &[String], ctx: &mut CliContext<'_>) -> Result<i32, CodedError> {
    let mut allowed = all_metadata_flags();
    allowed.push(("make-default", true));
    let (flags, positionals) = parse_flags(args, &allowed)?;
    if !positionals.is_empty() {
        return Err(usage_error(
            "instance add does not accept positional arguments.",
        ));
    }
    let mut initial = Object::new();
    for (flag, field) in METADATA_FLAGS {
        if let Some(v) = flags.get(flag)
            && field != "noDefault"
        {
            let v = if field == "redirectPort" {
                parse_redirect_port(v)?
            } else {
                v.clone()
            };
            initial.insert(field.into(), v);
        }
    }
    if let Some(v) = flags.get("default") {
        initial.insert("default".into(), parse_boolean(v, "default")?.into());
    }
    if flags.contains_key("no-default") {
        initial.insert("default".into(), false.into());
    }
    let mut instance = prompt_metadata(ctx, &initial).await?;
    let name = match instance.get("name") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let grant = effective_grant_type(&instance).to_owned();
    let auth = str_of(&instance, "authType").unwrap_or("").to_owned();
    let (password_ref, secret_ref) = if auth == "basic" {
        (Some(to_ref(CredentialType::Password, &name)?), None)
    } else if grant == "client_credentials" {
        (None, Some(to_ref(CredentialType::ClientSecret, &name)?))
    } else if grant == "password" {
        (
            Some(to_ref(CredentialType::Password, &name)?),
            Some(to_ref(CredentialType::ClientSecret, &name)?),
        )
    } else {
        (None, None)
    };
    let credential_ref = if auth == "oauth" && grant == "password" {
        Some(json!({"password": password_ref, "clientSecret": secret_ref}))
    } else {
        password_ref
            .clone()
            .or_else(|| secret_ref.clone())
            .map(Value::from)
    };
    let mut candidate = instance.clone();
    if let Some(r) = &credential_ref {
        candidate.insert("credentialRef".into(), r.clone());
    }
    if ctx
        .registry
        .list()?
        .iter()
        .any(|i| i.get("name").and_then(Value::as_str) == Some(name.as_str()))
    {
        return Err(CodedError::new(
            "INSTANCE_ALREADY_EXISTS",
            format!("Instance '{name}' already exists"),
        ));
    }
    ctx.registry.validate(&Value::Object(candidate))?;
    let origin = origin_of_instance(&instance)?;
    let mut snapshots = Vec::new();
    let result: Result<Object, CodedError> = async {
        if auth == "basic" {
            let secret = prompt_secret(ctx, "password", "Password:")?;
            let r = password_ref.clone().unwrap_or_default();
            set_with_snapshot(&ctx.credentials, &r, &secret, &origin, &mut snapshots).await?;
        } else if grant == "client_credentials" {
            let secret = prompt_secret(ctx, "clientSecret", "Client secret:")?;
            let r = secret_ref.clone().unwrap_or_default();
            set_with_snapshot(&ctx.credentials, &r, &secret, &origin, &mut snapshots).await?;
        } else if grant == "password" {
            let pw = prompt_secret(ctx, "password", "Password:")?;
            let cs = prompt_secret(ctx, "clientSecret", "Client secret:")?;
            let p = password_ref.clone().unwrap_or_default();
            let c = secret_ref.clone().unwrap_or_default();
            set_with_snapshot(&ctx.credentials, &p, &pw, &origin, &mut snapshots).await?;
            set_with_snapshot(&ctx.credentials, &c, &cs, &origin, &mut snapshots).await?;
        }
        if let Some(r) = &credential_ref {
            instance.insert("credentialRef".into(), r.clone());
        }
        let make_default = flags.get("make-default") == Some(&Value::Bool(true))
            || instance.get("default") == Some(&Value::Bool(true));
        ctx.registry
            .register(&Value::Object(instance.clone()), make_default)
    }
    .await;
    match result {
        Ok(registered) => {
            ctx.out(&format!("Added instance '{name}'."));
            ctx.out(&pretty(&redact(&Value::Object(registered), false)));
            ctx.err(&crate::updates_hint());
            Ok(0)
        }
        Err(e) => {
            restore_snapshots(ctx, &snapshots, "Credential rollback").await;
            Err(e)
        }
    }
}

fn list_command(ctx: &mut CliContext<'_>) -> Result<i32, CodedError> {
    // 6.x parity: `Array.map(redact)` passes the index as `insideCredentialRef`,
    // so every entry after the first keeps only password/clientSecret keys. The
    // entries are stripped of secrets first, so that quirk can never print one
    // even if a registry returned raw (legacy plaintext) entries.
    let mut instances: Vec<Value> = ctx
        .registry
        .list()?
        .into_iter()
        .enumerate()
        .map(|(i, o)| redact(&redact_secrets(&Value::Object(o)), i >= 1))
        .collect();
    let key = |v: &Value| match v.get("name") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "undefined".into(),
    };
    instances.sort_by_key(|a| key(a));
    if instances.is_empty() {
        ctx.out("No registered instances.");
        return Ok(0);
    }
    ctx.out(&pretty(&Value::Array(instances)));
    Ok(0)
}

async fn update_command(
    name: Option<&String>,
    args: &[String],
    ctx: &mut CliContext<'_>,
) -> Result<i32, CodedError> {
    let Some(name) = name else {
        return Err(usage_error("instance update requires an instance name."));
    };
    let (flags, positionals) = parse_flags(args, &all_metadata_flags())?;
    if !positionals.is_empty() {
        return Err(usage_error(
            "instance update accepts metadata options only.",
        ));
    }
    let existing = ctx.registry.get(name)?;
    let auth_change = || {
        usage_error(
            "Authentication changes require remove/re-add; use instance remove then instance add.",
        )
    };
    if let Some(v) = flags.get("auth-type")
        && Some(v) != existing.get("authType")
    {
        return Err(auth_change());
    }
    if let Some(v) = flags.get("grant-type")
        && v.as_str() != Some(effective_grant_type(&existing))
    {
        return Err(auth_change());
    }
    let fields = [
        "url",
        "authType",
        "grantType",
        "username",
        "clientId",
        "scope",
        "authorizeUrl",
        "tokenUrl",
        "redirectPort",
        "callbackPath",
        "description",
    ];
    let flag_for = |field: &str| {
        METADATA_FLAGS
            .iter()
            .find(|(_, f)| *f == field)
            .map(|(fl, _)| *fl)
    };
    let seen = fields
        .iter()
        .any(|f| flag_for(f).is_some_and(|fl| flags.contains_key(fl)));
    let mut patch = Object::new();
    if let Some(v) = flags.get("default") {
        patch.insert("default".into(), parse_boolean(v, "default")?.into());
    }
    if flags.contains_key("no-default") {
        patch.insert("default".into(), false.into());
    }
    for field in fields {
        if let Some(v) = flag_for(field).and_then(|fl| flags.get(fl)) {
            let v = if field == "redirectPort" {
                parse_redirect_port(v)?
            } else {
                v.clone()
            };
            patch.insert(field.into(), v);
        }
    }
    if !seen && !flags.contains_key("default") && !flags.contains_key("no-default") {
        let default_of = |k: &str| existing.get(k).cloned();
        let or_empty = |k: &str| {
            Some(match existing.get(k) {
                Some(v) if hpm_core::config::truthy(Some(v)) => v.clone(),
                _ => json!(""),
            })
        };
        let url = ctx.ask(
            PromptKind::Input,
            spec("url", "ServiceNow URL:", default_of("url")),
        )?;
        patch.insert("url".into(), url);
        if str_of(&existing, "authType") == Some("basic") {
            let u = ctx.ask(
                PromptKind::Input,
                spec("username", "Username:", default_of("username")),
            )?;
            patch.insert("username".into(), u);
        } else {
            let c = ctx.ask(
                PromptKind::Input,
                spec("clientId", "Client ID:", default_of("clientId")),
            )?;
            patch.insert("clientId".into(), c);
            let s = ctx.ask(
                PromptKind::Input,
                spec("scope", "OAuth scope:", or_empty("scope")),
            )?;
            patch.insert("scope".into(), s);
        }
        let d = ctx.ask(
            PromptKind::Input,
            spec("description", "Description:", or_empty("description")),
        )?;
        patch.insert("description".into(), d);
    }
    // JS `undefined` patch values (no answer, no default) are dropped by JSON.
    patch.retain(|_, v| !v.is_null());
    clear_changed_oauth_identity(ctx, &existing, &patch).await?;
    let updated = ctx.registry.update(name, &patch)?;
    ctx.out(&format!("Updated instance '{name}'."));
    ctx.out(&pretty(&redact(&Value::Object(updated), false)));
    Ok(0)
}

async fn credential_set_command(
    name: &str,
    args: &[String],
    ctx: &mut CliContext<'_>,
) -> Result<i32, CodedError> {
    let (flags, positionals) = parse_flags(args, &[("type", false)])?;
    if !positionals.is_empty() {
        return Err(usage_error("instance credential set accepts --type only."));
    }
    let instance = ctx.registry.get(name)?;
    let grant = effective_grant_type(&instance).to_owned();
    let auth = str_of(&instance, "authType").unwrap_or("").to_owned();
    let requested = match flags
        .get("type")
        .filter(|v| hpm_core::config::truthy(Some(v)))
    {
        Some(v) => v.clone(),
        None => {
            let mut choices: Vec<(&str, &str)> = Vec::new();
            if auth == "basic" || (auth == "oauth" && grant == "password") {
                choices.push(("Password", "password"));
            }
            if auth == "oauth" && grant != "authorization_code" {
                choices.push(("Client secret", "client-secret"));
            }
            ctx.ask(
                PromptKind::Select,
                select_spec("type", "Credential type:", None, &choices),
            )?
        }
    };
    let kind = requested
        .as_str()
        .and_then(|r| expected_credential_type(&instance, r))
        .ok_or_else(|| usage_error("Invalid credential type. Use --help for usage."))?;
    let reference = credential_ref_for(name, kind)?;
    let patch = credential_ref_patch(&instance, kind, &reference);
    ctx.registry
        .validate(&Value::Object(merged(&instance, &patch)))?;
    let origin = origin_of_instance(&instance)?;
    let snapshots = vec![snapshot_secret(&ctx.credentials, &reference).await?];
    let label = if kind == CredentialType::Password {
        "Password"
    } else {
        "Client secret"
    };
    let secret = prompt_secret(ctx, "secret", &format!("{label}:"))?;
    let result = async {
        ctx.credentials
            .set_secret(&reference, &secret, &origin)
            .await?;
        ctx.registry.update(name, &patch)
    }
    .await;
    if let Err(e) = result {
        restore_snapshots(ctx, &snapshots, "Credential rollback").await;
        return Err(e);
    }
    ctx.out(&format!(
        "Credential '{}' updated for instance '{name}'.",
        kind.as_str()
    ));
    Ok(0)
}

/// `instance credential bind <name>`: binds secrets saved without an origin
/// (6.x, or migrated before binding existed) to the origin of the URL now
/// registered under `name`, after the operator confirms that origin. A
/// secret already bound elsewhere is refused (re-save it with `set`).
async fn credential_bind_command(name: &str, ctx: &mut CliContext<'_>) -> Result<i32, CodedError> {
    let instance = ctx.registry.get(name)?;
    let origin = origin_of_instance(&instance)?;
    let mut refs: Vec<String> = Vec::new();
    for r in instance_credential_refs(&instance) {
        if !refs.contains(&r) && has_secret(&ctx.credentials, &r).await? {
            refs.push(r);
        }
    }
    if refs.is_empty() {
        return Err(CodedError::new(
            "CREDENTIAL_NOT_FOUND",
            format!("No saved credential to bind for instance '{name}'."),
        ));
    }
    let mut unbound = Vec::new();
    for r in refs {
        match ctx.credentials.bound_origin(&r).await? {
            Some(bound) if bound == origin => {}
            Some(bound) => {
                return Err(CodedError::new(
                    "CREDENTIAL_ORIGIN_MISMATCH",
                    format!(
                        "The saved credential for instance '{name}' is already bound to {bound}, not {origin}. Re-save it with: happy-platform-mcp instance credential set {name}"
                    ),
                ));
            }
            None => unbound.push(r),
        }
    }
    if unbound.is_empty() {
        ctx.out(&format!(
            "Credentials for instance '{name}' are already bound to {origin}."
        ));
        return Ok(0);
    }
    let confirmed = ctx.ask(
        PromptKind::Confirm,
        spec(
            "confirm",
            &format!(
                "Bind the saved credentials for '{name}' to {origin}? They will only ever be sent there."
            ),
            Some(json!(false)),
        ),
    )?;
    if !hpm_core::config::truthy(Some(&confirmed)) {
        ctx.out("Cancelled.");
        return Ok(0);
    }
    for r in &unbound {
        ctx.credentials.bind_origin(r, &origin).await?;
    }
    ctx.out(&format!(
        "Bound credentials for instance '{name}' to {origin}."
    ));
    Ok(0)
}

async fn remove_command(name: &str, ctx: &mut CliContext<'_>) -> Result<i32, CodedError> {
    let instance = ctx.registry.get(name)?;
    let confirmed = ctx.ask(
        PromptKind::Confirm,
        spec(
            "confirm",
            &format!("Remove instance '{name}'?"),
            Some(json!(false)),
        ),
    )?;
    if !hpm_core::config::truthy(Some(&confirmed)) {
        ctx.out("Cancelled.");
        return Ok(0);
    }
    if persists_refresh_tokens(&instance) {
        clear_refresh_tokens_everywhere(ctx, &instance).await?;
    }
    let mut refs: Vec<String> = Vec::new();
    for r in instance_credential_refs(&instance) {
        if !refs.contains(&r) {
            refs.push(r);
        }
    }
    let mut snapshots = Vec::new();
    for r in &refs {
        snapshots.push(snapshot_secret(&ctx.credentials, r).await?);
    }
    let mut deleted = 0usize;
    let mut result: Result<(), CodedError> = Ok(());
    for snapshot in &snapshots {
        if snapshot.existed {
            deleted += 1;
        }
        match ctx.credentials.delete_secret(&snapshot.reference).await {
            Ok(false) if snapshot.existed => {
                result = Err(CodedError::plain(format!(
                    "Credential deletion failed for {}",
                    snapshot.reference
                )));
                break;
            }
            Err(e) => {
                result = Err(e);
                break;
            }
            _ => {}
        }
    }
    if result.is_ok() {
        result = ctx.registry.remove(name);
    }
    if let Err(error) = result {
        let restore: Vec<Snapshot> = if deleted > 0 {
            snapshots
                .into_iter()
                .filter(|s| s.existed)
                .take(deleted)
                .collect()
        } else {
            snapshots
        };
        let failures = restore_snapshots(ctx, &restore, "Credential rollback").await;
        if failures.is_empty() {
            let line = format!(
                "Removal rollback completed; instance metadata remains registered. Original cause: {}",
                cli_error_message(&error)
            );
            ctx.err(&line);
        }
        return Err(error);
    }
    ctx.out(&format!("Removed instance '{name}'."));
    Ok(0)
}

async fn test_command(name: &str, ctx: &mut CliContext<'_>) -> Result<i32, CodedError> {
    let instance = ctx.registry.get(name)?;
    let safe = match redact(&Value::Object(instance), false) {
        Value::Object(o) => o,
        _ => Object::new(),
    };
    match ctx.probe.probe(&safe, &ctx.credentials).await {
        Ok(()) => {
            ctx.out(&format!("Instance test succeeded for '{name}'."));
            Ok(0)
        }
        Err(e) => {
            let line = match e.status {
                Some(401) => format!(
                    "Authentication failed for '{name}'. Check the credential with instance credential set."
                ),
                Some(403) => format!(
                    "Authorization failed for '{name}'. The credentials are valid but lack permission for the test read."
                ),
                _ => format!(
                    "Network/configuration failure testing '{name}': {}",
                    e.message
                ),
            };
            ctx.err(&line);
            Ok(1)
        }
    }
}

// --- migrate ---------------------------------------------------------------

const LEGACY_SECRET_KEYS: [&str; 7] = [
    "password",
    "clientSecret",
    "token",
    "refreshToken",
    "accessToken",
    "apiKey",
    "secret",
];

struct LegacyEntry {
    index: usize,
    kind: String,
    value: Zeroizing<String>,
}

fn legacy_secret_entries(document: &Value) -> Result<Vec<LegacyEntry>, CodedError> {
    let Some(obj) = document
        .as_object()
        .filter(|o| o.get("instances").is_some_and(Value::is_array))
    else {
        return Err(CodedError::plain(
            "Legacy instance registry is malformed; no changes were made.",
        ));
    };
    if obj.get("version").is_some_and(|v| *v != json!(1)) {
        return Err(CodedError::plain(
            "Legacy instance registry version is unsupported; no changes were made.",
        ));
    }
    fn walk(
        value: &Value,
        path: &str,
        instance: Option<usize>,
        out: &mut Vec<LegacyEntry>,
    ) -> Result<(), CodedError> {
        match value {
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    let idx = if path == "instances" {
                        Some(i)
                    } else {
                        instance
                    };
                    walk(item, &format!("{path}[{i}]"), idx, out)?;
                }
            }
            Value::Object(o) => {
                for (key, nested) in o {
                    if key == "credentialRef" {
                        continue;
                    }
                    if LEGACY_SECRET_KEYS.contains(&key.as_str()) {
                        let allowed = instance.is_some_and(|i| {
                            (key == "password" || key == "clientSecret")
                                && path == format!("instances[{i}]")
                        });
                        if !allowed {
                            return Err(CodedError::plain(format!(
                                "Legacy secret placement '{path}.{key}' is unsupported; no changes were made."
                            )));
                        }
                        let Some(s) = nested.as_str().filter(|s| !s.is_empty()) else {
                            return Err(CodedError::plain(format!(
                                "Legacy credential '{path}.{key}' is incomplete; no changes were made."
                            )));
                        };
                        out.push(LegacyEntry {
                            index: instance.unwrap_or(0),
                            kind: key.clone(),
                            value: Zeroizing::new(s.to_owned()),
                        });
                        continue;
                    }
                    let child = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(nested, &child, instance, out)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(document, "", None, &mut out)?;
    Ok(out)
}

struct MigrationWrite {
    reference: String,
    value: Zeroizing<String>,
    /// Origin of the entry's URL (validated before any write).
    origin: String,
}

fn label_of(instance: &Object, index: usize) -> String {
    match instance.get("name") {
        Some(v) if hpm_core::config::truthy(Some(v)) => v
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string()),
        _ => (index + 1).to_string(),
    }
}

fn nonblank(instance: &Object, key: &str) -> bool {
    str_of(instance, key).is_some_and(|s| !hpm_core::config::js_trim(s).is_empty())
}

fn build_migrated_document(
    document: &Value,
    registry: &dyn CliRegistry,
) -> Result<(Value, Vec<MigrationWrite>), CodedError> {
    let entries = legacy_secret_entries(document)?;
    let instances = document["instances"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut candidates = Vec::new();
    let mut writes = Vec::new();
    for (index, raw) in instances.iter().enumerate() {
        let Some(instance) = raw.as_object() else {
            return Err(CodedError::plain(format!(
                "Legacy instance {} is malformed; no changes were made.",
                index + 1
            )));
        };
        let auth = match instance.get("authType") {
            Some(v) if hpm_core::config::truthy(Some(v)) => v.as_str().unwrap_or("").to_owned(),
            _ => "basic".into(),
        };
        let grant = match instance.get("grantType") {
            Some(v) if hpm_core::config::truthy(Some(v)) => v.as_str().unwrap_or("").to_owned(),
            _ => {
                if hpm_core::config::truthy(instance.get("username")) {
                    "password".into()
                } else {
                    "client_credentials".into()
                }
            }
        };
        let label = label_of(instance, index);
        let find = |k: &str| entries.iter().find(|e| e.index == index && e.kind == k);
        let password = find("password");
        let client_secret = find("clientSecret");
        let fail = |m: String| Err(CodedError::plain(m));
        if auth != "basic" && auth != "oauth" {
            return fail(format!(
                "Legacy instance '{label}' has an unsupported authType; no changes were made."
            ));
        }
        if auth == "basic" && instance.contains_key("grantType") {
            return fail(format!(
                "Basic instance '{label}' has an unsupported grantType; no changes were made."
            ));
        }
        if auth == "oauth"
            && !["client_credentials", "password", "authorization_code"].contains(&grant.as_str())
        {
            return fail(format!(
                "OAuth instance '{label}' has an unsupported grantType; no changes were made."
            ));
        }
        let (Some(name), Some(_)) = (str_of(instance, "name"), str_of(instance, "url")) else {
            return fail(format!(
                "Legacy instance {} is malformed; no changes were made.",
                index + 1
            ));
        };
        if auth == "basic" && !nonblank(instance, "username") {
            return fail(format!(
                "Basic instance '{name}' is malformed; no changes were made."
            ));
        }
        if auth == "oauth" && !nonblank(instance, "clientId") {
            return fail(format!(
                "OAuth instance '{name}' is malformed; no changes were made."
            ));
        }
        if auth == "oauth" && grant == "password" && !nonblank(instance, "username") {
            return fail(format!(
                "OAuth password instance '{name}' is malformed; no changes were made."
            ));
        }
        if auth == "basic" && client_secret.is_some() {
            return fail(format!(
                "Basic instance '{name}' has an unsupported clientSecret; no changes were made."
            ));
        }
        if auth == "oauth"
            && grant == "authorization_code"
            && (password.is_some() || client_secret.is_some())
        {
            return fail(format!(
                "Confidential authorization-code instance '{name}' cannot be migrated safely; no changes were made."
            ));
        }
        if auth == "oauth" && grant == "client_credentials" && password.is_some() {
            return fail(format!(
                "OAuth client-credentials instance '{name}' has an unsupported password; no changes were made."
            ));
        }
        if auth == "oauth" && grant == "password" && password.is_some() != client_secret.is_some() {
            return fail(format!(
                "OAuth password instance '{name}' has incomplete plaintext credentials; no changes were made."
            ));
        }
        let mut candidate = instance.clone();
        // Bound with the secret at write time; an invalid URL fails
        // `validate` below before any write.
        let origin = str_of(instance, "url")
            .and_then(|u| instance_origin(u).ok())
            .unwrap_or_default();
        candidate.remove("password");
        candidate.remove("clientSecret");
        if auth == "oauth"
            && grant == "password"
            && password.is_none()
            && client_secret.is_none()
            && let Some(Value::Object(prior)) = candidate.get("credentialRef").cloned()
        {
            let pick = |a: &str, b: &str| {
                prior
                    .get(a)
                    .filter(|v| !v.is_null())
                    .or_else(|| prior.get(b))
                    .cloned()
            };
            let mut next = Object::new();
            if let Some(v) = pick("password", "passwordRef") {
                next.insert("password".into(), v);
            }
            if let Some(v) = pick("clientSecret", "clientSecretRef") {
                next.insert("clientSecret".into(), v);
            }
            candidate.insert("credentialRef".into(), Value::Object(next));
        }
        if let (true, Some(p)) = (auth == "basic", password) {
            let r = credential_ref_for(name, CredentialType::Password)?;
            candidate.insert("credentialRef".into(), r.clone().into());
            writes.push(MigrationWrite {
                reference: r,
                value: p.value.clone(),
                origin: origin.clone(),
            });
        } else if let (true, Some(c)) = (
            auth == "oauth" && grant == "client_credentials",
            client_secret,
        ) {
            let r = credential_ref_for(name, CredentialType::ClientSecret)?;
            candidate.insert("credentialRef".into(), r.clone().into());
            writes.push(MigrationWrite {
                reference: r,
                value: c.value.clone(),
                origin: origin.clone(),
            });
        } else if let (true, Some(p), Some(c)) = (
            auth == "oauth" && grant == "password",
            password,
            client_secret,
        ) {
            let pr = credential_ref_for(name, CredentialType::Password)?;
            let cr = credential_ref_for(name, CredentialType::ClientSecret)?;
            candidate.insert(
                "credentialRef".into(),
                json!({"password": pr, "clientSecret": cr}),
            );
            writes.push(MigrationWrite {
                reference: pr,
                value: p.value.clone(),
                origin: origin.clone(),
            });
            writes.push(MigrationWrite {
                reference: cr,
                value: c.value.clone(),
                origin: origin.clone(),
            });
        }
        registry.validate(&Value::Object(candidate.clone()))?;
        candidates.push(Value::Object(candidate));
    }
    let mut migrated = document.as_object().cloned().unwrap_or_default();
    migrated.insert("version".into(), json!(1));
    migrated.insert("instances".into(), Value::Array(candidates));
    let migrated = Value::Object(migrated);
    registry.validate_document(&migrated)?;
    Ok((migrated, writes))
}

fn migration_identity_error(read: &Path, write: &Path) -> CodedError {
    CodedError::new(
        "MIGRATION_SOURCE_IDENTITY_FAILED",
        "Migration refused: the plaintext source and registry target identities could not be established safely. Check file permissions and choose a distinct registry target; the source bytes and OS keychain were left unchanged.",
    )
    .with("readPath", read.to_string_lossy().into_owned())
    .with("writePath", write.to_string_lossy().into_owned())
}

fn canonical_migration_path(file: &Path, read: &Path, write: &Path) -> Result<PathBuf, CodedError> {
    let mut candidate = resolve_path(file);
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&candidate) {
            Ok(mut c) => {
                for m in missing.iter().rev() {
                    c.push(m);
                }
                return Ok(c);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(parent) = candidate.parent().map(Path::to_path_buf) else {
                    return Err(migration_identity_error(read, write));
                };
                missing.push(candidate.file_name().unwrap_or_default().to_owned());
                candidate = parent;
            }
            Err(_) => return Err(migration_identity_error(read, write)),
        }
    }
}

fn migration_source_equals_target(registry: &dyn CliRegistry) -> Result<bool, CodedError> {
    let read = registry.read_path();
    let write = registry.write_path();
    if read == write {
        return Ok(true);
    }
    Ok(canonical_migration_path(&read, &read, &write)?
        == canonical_migration_path(&write, &read, &write)?)
}

async fn migrate_command(ctx: &mut CliContext<'_>) -> Result<i32, CodedError> {
    let document = ctx.registry.raw_document()?;
    let (migrated, writes) = build_migrated_document(&document, ctx.registry.as_ref())?;
    if writes.is_empty() {
        ctx.out("No legacy plaintext credentials require migration.");
        return Ok(0);
    }
    if migration_source_equals_target(ctx.registry.as_ref())? {
        return Err(CodedError::new(
            "MIGRATION_SOURCE_EQUALS_TARGET",
            "Migration refused: the plaintext source and registry target resolve to the same file. Choose a distinct HAPPY_CONFIG_PATH target, or unset HAPPY_CONFIG_PATH to use automatic package-legacy -> user-registry migration. The source bytes and OS keychain were left unchanged.",
        )
        .with("readPath", ctx.registry.read_path().to_string_lossy().into_owned())
        .with("writePath", ctx.registry.write_path().to_string_lossy().into_owned()));
    }
    // Recorded deviation (review M2): name the plaintext source before asking.
    ctx.out(&format!(
        "{MIGRATION_SOURCE_PREFIX}{}",
        ctx.registry.read_path().display()
    ));
    let confirmed = ctx.ask(
        PromptKind::Confirm,
        spec(
            "confirm",
            "Migrate legacy credentials into the OS keychain?",
            Some(json!(false)),
        ),
    )?;
    if !hpm_core::config::truthy(Some(&confirmed)) {
        ctx.out("Cancelled.");
        return Ok(0);
    }
    let mut snapshots = Vec::new();
    let result: Result<(), CodedError> = async {
        for w in &writes {
            set_with_snapshot(
                &ctx.credentials,
                &w.reference,
                &w.value,
                &w.origin,
                &mut snapshots,
            )
            .await?;
        }
        ctx.registry.write_migrated_document(&migrated)
    }
    .await;
    if let Err(e) = result {
        restore_snapshots(ctx, &snapshots, "Migration rollback").await;
        return Err(e);
    }
    let n = migrated["instances"].as_array().map_or(0, Vec::len);
    ctx.out(&format!(
        "Migrated {n} instance{}; the legacy file was left untouched.",
        if n == 1 { "" } else { "s" }
    ));
    Ok(0)
}

// ---------------------------------------------------------------------------
// Entry

/// `runInstanceCli(argv)`: returns the process exit code.
pub async fn run_instance_cli(argv: &[String], ctx: &mut CliContext<'_>) -> i32 {
    if contains_secret_argument(argv) {
        ctx.err(SECRET_ARGUMENT_ERROR);
        return 2;
    }
    let args: &[String] = if argv.first().map(String::as_str) == Some("instance") {
        &argv[1..]
    } else {
        argv
    };
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        ctx.out(USAGE);
        return 0;
    }
    let result = match args[0].as_str() {
        "list" if args.len() != 1 => Err(usage_error("instance list does not accept options.")),
        "list" => list_command(ctx),
        "add" => add_command(&args[1..], ctx).await,
        "update" => update_command(args.get(1), args.get(2..).unwrap_or(&[]), ctx).await,
        "test" if args.len() != 2 => Err(usage_error(
            "instance test requires exactly one instance name.",
        )),
        "test" => test_command(&args[1], ctx).await,
        "remove" if args.len() != 2 => Err(usage_error(
            "instance remove requires exactly one instance name.",
        )),
        "remove" => remove_command(&args[1], ctx).await,
        "credential" if args.get(1).map(String::as_str) == Some("bind") && args.len() != 3 => Err(
            usage_error("instance credential bind requires exactly one instance name."),
        ),
        "credential" if args.get(1).map(String::as_str) == Some("bind") => {
            credential_bind_command(&args[2], ctx).await
        }
        "credential" if args.get(1).map(String::as_str) != Some("set") || args.len() < 3 => Err(
            usage_error("instance credential set requires an instance name."),
        ),
        "credential" => credential_set_command(&args[2], &args[3..], ctx).await,
        "migrate" if args.len() != 1 => {
            Err(usage_error("instance migrate does not accept options."))
        }
        "migrate" => migrate_command(ctx).await,
        _ => Err(usage_error(
            "Unknown instance command. Use --help for usage.",
        )),
    };
    match result {
        Ok(code) => code,
        Err(e) if e.code == "NON_INTERACTIVE_PROMPT" => {
            ctx.err(&e.message);
            2
        }
        Err(e)
            if e.code == "MIGRATION_SOURCE_EQUALS_TARGET"
                || e.code == "MIGRATION_SOURCE_IDENTITY_FAILED" =>
        {
            ctx.err(&safe_migration_error(&e));
            2
        }
        Err(e) if e.code == "USAGE" => {
            ctx.err(&format!("{}\n{USAGE}", e.message));
            2
        }
        Err(e) => {
            ctx.err(&cli_error_message(&e));
            1
        }
    }
}

#[cfg(test)]
#[path = "instance_cli_tests.rs"]
mod tests;
