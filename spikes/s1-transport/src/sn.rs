//! Minimal ServiceNow client for the S1 spike: Basic auth only, credential
//! resolution mirroring 6.2.0 (`src/config-manager.js`, `src/instance-credential-store.js`).
//!
//! Precedence (spike subset of 6.2.0):
//! 1. `SERVICENOW_INSTANCE_URL` + `SERVICENOW_USERNAME` + `SERVICENOW_PASSWORD` env.
//! 2. Registry `HAPPY_CONFIG_PATH` else `~/.config/happy-platform-mcp/instances.json`,
//!    instance `SERVICENOW_INSTANCE` else the `default: true` entry; password from
//!    keychain service `happy-platform-mcp`, account = `credentialRef` verbatim.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

pub const KEYCHAIN_SERVICE: &str = "happy-platform-mcp";

#[derive(Clone)]
pub struct Instance {
    pub name: String,
    pub url: String,
    username: String,
    password: String,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance")
            .field("name", &self.name)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Registry {
    instances: Vec<RegistryEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryEntry {
    name: String,
    url: String,
    #[serde(default)]
    auth_type: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    credential_ref: Option<String>,
    #[serde(default)]
    default: bool,
}

fn registry_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HAPPY_CONFIG_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".config/happy-platform-mcp/instances.json")
}

fn keychain_secret(account: &str) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        let store = apple_native_keyring_store::keychain::Store::new()
            .map_err(|e| format!("keychain unavailable: {e}"))?;
        keyring_core::set_default_store(store);
    }
    let entry = keyring_core::Entry::new(KEYCHAIN_SERVICE, account)
        .map_err(|e| format!("keychain entry: {e}"))?;
    let value = entry
        .get_password()
        .map_err(|_| format!("Credential not found for {account}"))?;
    if value.trim().is_empty() {
        return Err(format!("Credential not found for {account}"));
    }
    Ok(value)
}

pub fn resolve_instance() -> Result<Instance, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let (Some(url), Some(username), Some(password)) = (
        env("SERVICENOW_INSTANCE_URL"),
        env("SERVICENOW_USERNAME"),
        env("SERVICENOW_PASSWORD"),
    ) {
        return Ok(Instance {
            name: "default".into(),
            url: url.trim_end_matches('/').into(),
            username,
            password,
        });
    }
    let path = registry_path();
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("registry {}: {e}", path.display()))?;
    let registry: Registry = serde_json::from_str(&raw).map_err(|e| format!("registry parse: {e}"))?;
    let wanted = env("SERVICENOW_INSTANCE");
    let entry = registry
        .instances
        .into_iter()
        .find(|i| match &wanted {
            Some(name) => &i.name == name,
            None => i.default,
        })
        .ok_or_else(|| format!("instance {:?} not in registry", wanted.as_deref().unwrap_or("<default>")))?;
    if entry.auth_type.as_deref().is_some_and(|t| t != "basic") {
        return Err(format!("spike supports basic auth only (instance {})", entry.name));
    }
    let password = match (entry.password, entry.credential_ref) {
        (_, Some(r)) => keychain_secret(&r)?,
        (Some(p), None) => p,
        (None, None) => return Err(format!("no credential for instance {}", entry.name)),
    };
    Ok(Instance {
        name: entry.name,
        url: entry.url.trim_end_matches('/').into(),
        username: entry.username.unwrap_or_default(),
        password,
    })
}

/// Arguments of SN-Query-Table, typed after `src/mcp-server-consolidated.js:1630-1652`.
#[derive(Debug, Deserialize)]
pub struct QueryTableArgs {
    pub table_name: String,
    pub query: Option<String>,
    pub fields: Option<String>,
    pub limit: Option<Value>,
    pub offset: Option<Value>,
    pub order_by: Option<String>,
}

/// JS truthiness for the params `getRecords` appends (`src/servicenow-client.js:1276-1286`).
fn js_param(v: &Value) -> Option<String> {
    match v {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) if s.is_empty() => None,
        Value::Number(n) if n.as_f64() == Some(0.0) => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

pub struct Client {
    http: reqwest::Client,
    instance: Instance,
}

impl Client {
    pub fn new(instance: Instance) -> Self {
        Self { http: reqwest::Client::new(), instance }
    }

    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// `getRecords` port. Param order matches 6.2.0: query, limit, fields, offset, order_by.
    pub async fn get_records(&self, a: &QueryTableArgs) -> Result<Vec<Value>, String> {
        let limit = a.limit.clone().unwrap_or(Value::from(25));
        let candidates = [
            ("sysparm_query", a.query.clone().map(Value::String)),
            ("sysparm_limit", Some(limit)),
            ("sysparm_fields", a.fields.clone().map(Value::String)),
            ("sysparm_offset", a.offset.clone()),
            ("sysparm_order_by", a.order_by.clone().map(Value::String)),
        ];
        let params: Vec<(&str, String)> = candidates
            .into_iter()
            .filter_map(|(k, v)| v.as_ref().and_then(js_param).map(|v| (k, v)))
            .collect();
        let url = format!("{}/api/now/table/{}", self.instance.url, a.table_name);
        let resp = self
            .http
            .get(url)
            .query(&params)
            .basic_auth(&self.instance.username, Some(&self.instance.password))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e.without_url()))?;
        let status = resp.status();
        if !status.is_success() {
            // Diagnostic only (stderr); the tool text keeps the axios message shape.
            let body = resp.text().await.unwrap_or_default();
            eprintln!("[hpm-s1] ServiceNow HTTP {}: {}", status.as_u16(), body.chars().take(300).collect::<String>());
            return Err(format!("Request failed with status code {}", status.as_u16()));
        }
        let body: Value = resp.json().await.map_err(|e| format!("invalid JSON: {e}"))?;
        match body.get("result") {
            Some(Value::Array(rows)) => Ok(rows.clone()),
            _ => Err("unexpected response shape".into()),
        }
    }
}
