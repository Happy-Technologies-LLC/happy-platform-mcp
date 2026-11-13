//! Single-user `hpm_types::registry::RegistryStore` over `instances.json` (lane A).
//! Ports `src/instance-registry.js` and `src/oauth-endpoint-policy.js`.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};
use url::Url;

use crate::config::{
    CodedError, ConfigSource, Env, MAX_URL_LENGTH, Object, exceeds_url_length, home_dir, js_trim,
    resolve_config_paths, resolve_path, trim_trailing_slashes, truthy,
};
use crate::keychain::{
    CredentialType, credential_ref_for, is_valid_instance_name, parse_credential_ref,
};

// ---------------------------------------------------------------------------
// oauth-endpoint-policy.js

pub const TRUSTED_OAUTH_ORIGINS_ENV: &str = "SERVICENOW_OAUTH_TRUSTED_ORIGINS";

fn loopback_host(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

fn allowed_scheme(url: &Url) -> bool {
    url.scheme() == "https" || (url.scheme() == "http" && loopback_host(url))
}

fn has_credentials(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some_and(|p| !p.is_empty())
}

fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

fn invalid_trusted_origins(position: usize) -> CodedError {
    CodedError::new(
        "OAUTH_TRUSTED_ORIGINS_INVALID",
        format!(
            "{TRUSTED_OAUTH_ORIGINS_ENV} entry {position} must be an exact https:// origin (or loopback http://) with no path, query, fragment, credentials, or wildcard"
        ),
    )
}

/// Operator allow-list: comma-separated exact origins. Malformed entries fail loudly.
pub fn parse_trusted_oauth_origins(value: Option<&str>) -> Result<BTreeSet<String>, CodedError> {
    let mut origins = BTreeSet::new();
    let Some(value) = value else {
        return Ok(origins);
    };
    for (index, raw) in value.split(',').enumerate() {
        let entry = js_trim(raw);
        if entry.is_empty() {
            continue;
        }
        if exceeds_url_length(entry) {
            return Err(invalid_trusted_origins(index + 1));
        }
        let parsed = Url::parse(entry).map_err(|_| invalid_trusted_origins(index + 1))?;
        if entry.contains('*')
            || entry.contains('?')
            || entry.contains('#')
            || has_credentials(&parsed)
            || parsed.path() != "/"
            || parsed.query().is_some_and(|q| !q.is_empty())
            || parsed.fragment().is_some_and(|f| !f.is_empty())
            || !allowed_scheme(&parsed)
        {
            return Err(invalid_trusted_origins(index + 1));
        }
        origins.insert(origin(&parsed));
    }
    Ok(origins)
}

/// Effective authorize/token endpoints (JS `||`: empty falls back to the default).
pub fn resolve_oauth_endpoints(
    instance_url: &str,
    authorize_url: Option<&str>,
    token_url: Option<&str>,
) -> (String, String) {
    (
        authorize_url
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{instance_url}/oauth_auth.do")),
        token_url
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{instance_url}/oauth_token.do")),
    )
}

fn endpoint_error(message: String, field: &str) -> CodedError {
    CodedError::new("OAUTH_ENDPOINT_NOT_APPROVED", message).with("field", field)
}

/// Requires an OAuth endpoint to share the instance origin or to be listed in the
/// operator allow-list (environment only). Returns the normalized href.
pub fn assert_approved_oauth_endpoint(
    endpoint: &str,
    instance_url: &str,
    field: &str,
    env: &Env,
) -> Result<String, CodedError> {
    let trusted = parse_trusted_oauth_origins(env.get(TRUSTED_OAUTH_ORIGINS_ENV).as_deref())?;
    if exceeds_url_length(endpoint) || exceeds_url_length(instance_url) {
        return Err(endpoint_error(
            format!("{field} and the instance URL must be at most {MAX_URL_LENGTH} characters"),
            field,
        ));
    }
    let (Ok(parsed), Ok(instance)) = (Url::parse(endpoint), Url::parse(instance_url)) else {
        return Err(endpoint_error(
            format!("{field} must be a valid URL"),
            field,
        ));
    };
    if has_credentials(&parsed)
        || parsed.fragment().is_some_and(|f| !f.is_empty())
        || !allowed_scheme(&parsed)
    {
        return Err(endpoint_error(
            format!(
                "{field} must be an HTTPS URL (or loopback HTTP) without credentials or fragment"
            ),
            field,
        ));
    }
    let endpoint_origin = origin(&parsed);
    if endpoint_origin == origin(&instance) || trusted.contains(&endpoint_origin) {
        return Ok(parsed.as_str().to_owned());
    }
    Err(endpoint_error(
        format!(
            "{field} origin {endpoint_origin} is not the instance origin and is not listed in {TRUSTED_OAUTH_ORIGINS_ENV}"
        ),
        field,
    ))
}

// ---------------------------------------------------------------------------
// Validation and redaction

const ALLOWED_FIELDS: [&str; 14] = [
    "name",
    "url",
    "authType",
    "grantType",
    "username",
    "clientId",
    "credentialRef",
    "scope",
    "authorizeUrl",
    "tokenUrl",
    "redirectPort",
    "callbackPath",
    "default",
    "description",
];
const LEGACY_SECRET_FIELDS: [&str; 2] = ["password", "clientSecret"];
const CANONICAL_CREDENTIAL_FIELDS: [&str; 2] = ["password", "clientSecret"];

pub fn is_secret_shaped_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .collect::<String>()
        .to_lowercase();
    if normalized == "tokenurl" || normalized == "credentialref" {
        return false;
    }
    [
        "privatekey",
        "clientsecret",
        "secret",
        "password",
        "accesstoken",
        "refreshtoken",
        "apikey",
        "token",
    ]
    .iter()
    .any(|w| normalized.contains(w))
}

fn is_canonical_ref(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| parse_credential_ref(s).is_ok())
}

fn redact_credential_reference(value: &Value) -> Option<Value> {
    if is_canonical_ref(value) {
        return Some(value.clone());
    }
    let obj = value.as_object()?;
    let mut result = Object::new();
    for key in CANONICAL_CREDENTIAL_FIELDS {
        let Some(v) = obj.get(key) else { continue };
        if !is_canonical_ref(v) {
            return None;
        }
        result.insert(key.into(), v.clone());
    }
    (!result.is_empty()).then_some(Value::Object(result))
}

fn effective_grant_type(o: &Object) -> Option<Value> {
    match o.get("grantType") {
        Some(v) => Some(v.clone()),
        None => Some(if truthy(o.get("username")) {
            "password".into()
        } else {
            "client_credentials".into()
        }),
    }
}

fn is_valid_credential_ref(instance: &Object) -> bool {
    if !instance.contains_key("credentialRef") {
        return true;
    }
    let name = instance.get("name").and_then(Value::as_str).unwrap_or("");
    let auth_type = instance
        .get("authType")
        .filter(|v| truthy(Some(v)))
        .and_then(Value::as_str)
        .unwrap_or("basic");
    let r = instance.get("credentialRef");
    if auth_type == "basic" {
        return canonical_credential_ref(r, name, CredentialType::Password, "credentialRef")
            .is_ok();
    }
    match effective_grant_type(instance)
        .as_ref()
        .and_then(Value::as_str)
    {
        Some("authorization_code") => false,
        Some("client_credentials") => {
            canonical_credential_ref(r, name, CredentialType::ClientSecret, "credentialRef").is_ok()
        }
        _ => r.is_some_and(|r| password_grant_refs(r, name, false).is_ok()),
    }
}

fn redact_instance_secrets(instance: &Value) -> Value {
    let Some(obj) = instance.as_object() else {
        return instance.clone();
    };
    let mut redacted = redact_secrets(instance);
    if !is_valid_credential_ref(obj)
        && let Some(r) = redacted.as_object_mut()
    {
        r.remove("credentialRef");
    }
    redacted
}

/// Removes secret-shaped keys and non-canonical credential references, recursively.
pub fn redact_secrets(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(redact_secrets).collect()),
        Value::Object(obj) => {
            let mut out = Object::new();
            for (key, nested) in obj {
                if is_secret_shaped_key(key) {
                    continue;
                }
                if key == "credentialRef" {
                    if let Some(r) = redact_credential_reference(nested) {
                        out.insert(key.clone(), r);
                    }
                    continue;
                }
                if key == "instances"
                    && let Value::Array(items) = nested
                {
                    out.insert(
                        key.clone(),
                        Value::Array(items.iter().map(redact_instance_secrets).collect()),
                    );
                    continue;
                }
                out.insert(key.clone(), redact_secrets(nested));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn has_legacy_instance_secrets(document: &Value) -> bool {
    document
        .get("instances")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|i| {
                let Some(o) = i.as_object() else { return false };
                let r = o.get("credentialRef").and_then(Value::as_object);
                o.contains_key("password")
                    || o.contains_key("clientSecret")
                    || r.is_some_and(|r| {
                        r.contains_key("passwordRef") || r.contains_key("clientSecretRef")
                    })
            })
        })
}

fn invalid(message: impl Into<String>, field: Option<&str>) -> CodedError {
    let e = CodedError::new("INVALID_INSTANCE_CONFIG", message);
    match field {
        Some(f) => e.with("field", f),
        None => e,
    }
}

fn normalize_credential_ref_aliases(
    value: &Value,
    allow_aliases: bool,
) -> Result<Value, CodedError> {
    let Some(obj) = value.as_object() else {
        return Ok(value.clone());
    };
    let mut normalized = obj.clone();
    for (canonical, alias) in [
        ("password", "passwordRef"),
        ("clientSecret", "clientSecretRef"),
    ] {
        let Some(alias_value) = normalized.get(alias).cloned() else {
            continue;
        };
        if let Some(existing) = normalized.get(canonical) {
            if *existing != alias_value {
                return Err(invalid(
                    format!("Conflicting credential references '{canonical}' and '{alias}'"),
                    Some(&format!("credentialRef.{canonical}")),
                ));
            }
        } else if !allow_aliases {
            return Err(invalid(
                format!("Credential reference alias '{alias}' is not accepted"),
                Some(&format!("credentialRef.{alias}")),
            ));
        } else {
            normalized.insert(canonical.into(), alias_value);
        }
        normalized.remove(alias);
    }
    Ok(Value::Object(normalized))
}

fn normalize_instance_credential_refs(
    instance: &Value,
    allow_aliases: bool,
) -> Result<Value, CodedError> {
    let Some(obj) = instance.as_object() else {
        return Ok(instance.clone());
    };
    let mut out = obj.clone();
    if let Some(r) = obj.get("credentialRef") {
        out.insert(
            "credentialRef".into(),
            normalize_credential_ref_aliases(r, allow_aliases)?,
        );
    }
    Ok(Value::Object(out))
}

fn unknown_name(name: &str) -> &str {
    if name.is_empty() { "unknown" } else { name }
}

fn assert_url_length(value: Option<&Value>, name: &str, field: &str) -> Result<(), CodedError> {
    if value
        .and_then(Value::as_str)
        .is_some_and(exceeds_url_length)
    {
        return Err(invalid(
            format!(
                "Instance '{}' {field} must be at most {MAX_URL_LENGTH} characters",
                unknown_name(name)
            ),
            Some(field),
        ));
    }
    Ok(())
}

/// `${origin}${pathname without trailing '/'}`.
pub fn canonicalize_instance_url(url: &str) -> Result<String, CodedError> {
    if exceeds_url_length(url) {
        return Err(invalid(
            format!("Instance 'unknown' url must be at most {MAX_URL_LENGTH} characters"),
            Some("url"),
        ));
    }
    let parsed = Url::parse(url).map_err(|_| CodedError::plain("Invalid URL"))?;
    Ok(format!(
        "{}{}",
        origin(&parsed),
        trim_trailing_slashes(parsed.path())
    ))
}

fn is_loopback_name(parsed: &Url) -> bool {
    let host = parsed.host_str().unwrap_or("");
    let host = host.strip_prefix('[').unwrap_or(host);
    let host = host.strip_suffix(']').unwrap_or(host).to_lowercase();
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1")
}

fn validate_url(url: Option<&Value>, name: &str) -> Result<(), CodedError> {
    assert_url_length(url, name, "url")?;
    let n = unknown_name(name);
    let Some(url) = url
        .and_then(Value::as_str)
        .filter(|u| !js_trim(u).is_empty())
    else {
        return Err(invalid(
            format!("Instance '{n}' requires a URL"),
            Some("url"),
        ));
    };
    let parsed = Url::parse(url)
        .map_err(|_| invalid(format!("Instance '{n}' has an invalid URL"), Some("url")))?;
    if has_credentials(&parsed) {
        return Err(invalid(
            format!("Instance '{n}' URL must not include username or password"),
            Some("url"),
        ));
    }
    if url.contains('?') || url.contains('#') {
        return Err(invalid(
            format!("Instance '{n}' URL must not include a query or fragment"),
            Some("url"),
        ));
    }
    if parsed.scheme() == "https" {
        return Ok(());
    }
    if parsed.scheme() != "http" || !is_loopback_name(&parsed) {
        return Err(invalid(
            format!("Instance '{n}' must use HTTPS except for loopback HTTP"),
            Some("url"),
        ));
    }
    Ok(())
}

fn validate_optional_url(value: &Value, name: &str, field: &str) -> Result<(), CodedError> {
    assert_url_length(Some(value), name, field)?;
    let Some(s) = value.as_str().filter(|s| !js_trim(s).is_empty()) else {
        return Err(invalid(
            format!("Instance '{name}' {field} must be a non-empty URL"),
            Some(field),
        ));
    };
    let parsed = Url::parse(s).map_err(|_| {
        invalid(
            format!("Instance '{name}' {field} must be a valid URL"),
            Some(field),
        )
    })?;
    if has_credentials(&parsed) {
        return Err(invalid(
            format!("Instance '{name}' {field} URL must not include username or password"),
            Some(field),
        ));
    }
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && is_loopback_name(&parsed)) {
        return Err(invalid(
            format!("Instance '{name}' {field} must use HTTPS except for loopback HTTP"),
            Some(field),
        ));
    }
    Ok(())
}

fn non_empty_string(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str)
        .is_some_and(|s| !js_trim(s).is_empty())
}

fn canonical_credential_ref(
    value: Option<&Value>,
    name: &str,
    kind: CredentialType,
    field: &str,
) -> Result<String, CodedError> {
    let Some(s) = value
        .and_then(Value::as_str)
        .filter(|s| !js_trim(s).is_empty())
    else {
        return Err(invalid(
            format!(
                "Instance credential reference '{field}' must be a non-empty canonical reference"
            ),
            Some(field),
        ));
    };
    let parsed = parse_credential_ref(s).map_err(|_| {
        invalid(
            format!("Instance credential reference '{field}' is invalid"),
            Some(field),
        )
    })?;
    let expected = credential_ref_for(name, kind).ok();
    if Some(&parsed.reference) != expected.as_ref()
        || parsed.instance_name != name
        || parsed.kind != kind
    {
        return Err(invalid(
            format!("Instance credential reference '{field}' does not match the instance"),
            Some(field),
        ));
    }
    Ok(parsed.reference)
}

fn password_grant_refs(
    value: &Value,
    name: &str,
    allow_aliases: bool,
) -> Result<(String, String), CodedError> {
    if !value.is_object() {
        return Err(invalid(
            "OAuth password grant requires credentialRef.password and credentialRef.clientSecret",
            Some("credentialRef"),
        ));
    }
    let normalized = normalize_credential_ref_aliases(value, allow_aliases)?;
    let obj = normalized.as_object().cloned().unwrap_or_default();
    for key in obj.keys() {
        if !CANONICAL_CREDENTIAL_FIELDS.contains(&key.as_str()) {
            return Err(invalid(
                format!("Unknown credential reference field '{key}'"),
                Some("credentialRef"),
            ));
        }
    }
    Ok((
        canonical_credential_ref(
            obj.get("password"),
            name,
            CredentialType::Password,
            "credentialRef.password",
        )?,
        canonical_credential_ref(
            obj.get("clientSecret"),
            name,
            CredentialType::ClientSecret,
            "credentialRef.clientSecret",
        )?,
    ))
}

fn is_valid_port(v: &Value) -> bool {
    v.as_f64()
        .is_some_and(|f| f.fract() == 0.0 && (0.0..=65535.0).contains(&f))
}

/// `validateNewInstance`. `allow_secrets` admits legacy plaintext `password`/`clientSecret`
/// and relaxes the per-branch field restrictions.
pub fn validate_new_instance(
    instance: &Value,
    allow_secrets: bool,
    env: &Env,
) -> Result<(), CodedError> {
    let Some(o) = instance.as_object() else {
        return Err(invalid("Instance configuration must be an object", None));
    };
    let strict = !allow_secrets;
    for field in o.keys() {
        if !ALLOWED_FIELDS.contains(&field.as_str())
            && !(allow_secrets && LEGACY_SECRET_FIELDS.contains(&field.as_str()))
        {
            return Err(invalid(
                format!("Unknown instance field '{field}'"),
                Some(field),
            ));
        }
    }
    let raw_name = o.get("name").and_then(Value::as_str);
    let name = raw_name.map(js_trim).unwrap_or("");
    if name.is_empty() || !is_valid_instance_name(name) || Some(name) != raw_name {
        return Err(invalid(
            "Instance name must be a trimmed identifier containing only letters, numbers, underscores, and hyphens",
            Some("name"),
        ));
    }
    validate_url(o.get("url"), name)?;
    let auth_type = match o.get("authType") {
        None => "basic",
        Some(v) => match v.as_str() {
            Some(s @ ("basic" | "oauth")) => s,
            _ => {
                return Err(invalid(
                    format!("Instance '{name}' has an unsupported authType"),
                    Some("authType"),
                ));
            }
        },
    };
    if o.get("default").is_some_and(|d| !d.is_boolean()) {
        return Err(invalid(
            format!("Instance '{name}' default must be a boolean"),
            Some("default"),
        ));
    }
    for field in ["scope", "description"] {
        if o.get(field).is_some_and(|v| !v.is_string()) {
            return Err(invalid(
                format!("Instance '{name}' {field} must be a string"),
                Some(field),
            ));
        }
    }
    for field in ["authorizeUrl", "tokenUrl"] {
        if let Some(v) = o.get(field) {
            validate_optional_url(v, name, field)?;
        }
    }
    let url = o.get("url").and_then(Value::as_str).unwrap_or("");
    for field in ["authorizeUrl", "tokenUrl"] {
        if let Some(v) = o.get(field).and_then(Value::as_str) {
            assert_approved_oauth_endpoint(v, url, field, env)
                .map_err(|e| invalid(format!("Instance '{name}' {}", e.message), Some(field)))?;
        }
    }
    if let Some(v) = o.get("callbackPath") {
        assert_url_length(Some(v), name, "callbackPath")?;
        let ok = v.as_str().is_some_and(|s| {
            !js_trim(s).is_empty()
                && s.starts_with('/')
                && !s.starts_with("//")
                && !s.contains('?')
                && !s.contains('#')
        });
        if !ok {
            return Err(invalid(
                format!("Instance '{name}' callbackPath must be a valid absolute path"),
                Some("callbackPath"),
            ));
        }
    }
    if o.get("redirectPort").is_some_and(|v| !is_valid_port(v)) {
        return Err(invalid(
            format!("Instance '{name}' redirectPort must be an integer from 0 to 65535"),
            Some("redirectPort"),
        ));
    }
    let mut non_empty_fields = vec!["username", "clientId"];
    if allow_secrets {
        non_empty_fields.extend(["password", "clientSecret"]);
    }
    for field in non_empty_fields {
        if o.contains_key(field) && !non_empty_string(o.get(field)) {
            return Err(invalid(
                format!("Instance '{name}' {field} must be a non-empty string"),
                Some(field),
            ));
        }
    }
    let present = |k: &str| o.contains_key(k);

    if auth_type == "basic" {
        if present("grantType") {
            return Err(invalid(
                format!("Basic instance '{name}' cannot specify grantType"),
                Some("grantType"),
            ));
        }
        if strict {
            for field in [
                "clientId",
                "scope",
                "authorizeUrl",
                "tokenUrl",
                "redirectPort",
                "callbackPath",
            ] {
                if present(field) {
                    return Err(invalid(
                        format!("Basic instance '{name}' cannot specify {field}"),
                        Some(field),
                    ));
                }
            }
        }
        if !non_empty_string(o.get("username")) {
            return Err(invalid(
                format!("Basic instance '{name}' requires username as a non-empty string"),
                Some("username"),
            ));
        }
        if present("credentialRef") {
            canonical_credential_ref(
                o.get("credentialRef"),
                name,
                CredentialType::Password,
                "credentialRef",
            )?;
        }
        if allow_secrets && truthy(o.get("password")) {
            return Ok(());
        }
        canonical_credential_ref(
            o.get("credentialRef"),
            name,
            CredentialType::Password,
            "credentialRef",
        )?;
        return Ok(());
    }

    let grant = effective_grant_type(o);
    let grant = match grant.as_ref().and_then(Value::as_str) {
        Some(g @ ("client_credentials" | "password" | "authorization_code")) => g,
        _ => {
            return Err(invalid(
                format!("OAuth instance '{name}' has an unsupported grantType"),
                Some("grantType"),
            ));
        }
    };
    if !non_empty_string(o.get("clientId")) {
        return Err(invalid(
            format!("OAuth instance '{name}' requires clientId as a non-empty string"),
            Some("clientId"),
        ));
    }
    if grant == "authorization_code" {
        if strict && present("username") {
            return Err(invalid(
                format!("Authorization-code instance '{name}' cannot specify username"),
                Some("username"),
            ));
        }
        if present("credentialRef") {
            return Err(invalid(
                format!(
                    "Public authorization-code instance '{name}' must not specify credentialRef"
                ),
                Some("credentialRef"),
            ));
        }
        return Ok(());
    }
    if grant == "client_credentials" {
        if strict {
            for field in ["username", "authorizeUrl", "redirectPort", "callbackPath"] {
                if present(field) {
                    return Err(invalid(
                        format!("Client-credentials instance '{name}' cannot specify {field}"),
                        Some(field),
                    ));
                }
            }
        }
        if present("credentialRef") {
            canonical_credential_ref(
                o.get("credentialRef"),
                name,
                CredentialType::ClientSecret,
                "credentialRef",
            )?;
        }
        if allow_secrets && truthy(o.get("clientSecret")) {
            return Ok(());
        }
        canonical_credential_ref(
            o.get("credentialRef"),
            name,
            CredentialType::ClientSecret,
            "credentialRef",
        )?;
        return Ok(());
    }
    if strict {
        for field in ["authorizeUrl", "redirectPort", "callbackPath"] {
            if present(field) {
                return Err(invalid(
                    format!("Password-grant instance '{name}' cannot specify {field}"),
                    Some(field),
                ));
            }
        }
    }
    if !non_empty_string(o.get("username")) {
        return Err(invalid(
            format!("OAuth password instance '{name}' requires username as a non-empty string"),
            Some("username"),
        ));
    }
    let no_ref = Value::Null;
    let r = o.get("credentialRef");
    if present("credentialRef") {
        password_grant_refs(r.unwrap_or(&no_ref), name, allow_secrets)?;
    }
    if allow_secrets && truthy(o.get("password")) && truthy(o.get("clientSecret")) {
        return Ok(());
    }
    password_grant_refs(r.unwrap_or(&no_ref), name, allow_secrets)?;
    Ok(())
}

fn reload_failed(message: impl Into<String>) -> CodedError {
    CodedError::new("REGISTRY_RELOAD_FAILED", message)
}

/// `validateDocument`: returns the alias-normalized document.
pub fn validate_document(document: &Value, env: &Env) -> Result<Value, CodedError> {
    let Some(doc) = document.as_object() else {
        return Err(reload_failed(
            "Instance registry document must be an object",
        ));
    };
    if let Some(version) = doc.get("version")
        && version.as_f64() != Some(1.0)
    {
        return Err(
            reload_failed("Unsupported instance registry version").with("version", version.clone())
        );
    }
    let Some(items) = doc.get("instances").and_then(Value::as_array) else {
        return Err(reload_failed(
            "Instance registry instances must be an array",
        ));
    };
    let legacy = has_legacy_instance_secrets(document);
    let normalized: Vec<Value> = items
        .iter()
        .map(|i| normalize_instance_credential_refs(i, legacy))
        .collect::<Result<_, _>>()?;
    let mut seen: Vec<Value> = Vec::new();
    let mut defaults = 0;
    for instance in &normalized {
        let name = instance.get("name").cloned().unwrap_or(Value::Null);
        if seen.contains(&name) {
            let shown = name.as_str().map(str::to_owned).unwrap_or_else(|| {
                if name.is_null() {
                    "undefined".into()
                } else {
                    name.to_string()
                }
            });
            return Err(
                reload_failed(format!("Duplicate instance name '{shown}' in registry"))
                    .with("field", "name"),
            );
        }
        seen.push(name);
        if instance.get("default") == Some(&Value::Bool(true)) {
            defaults += 1;
            if defaults > 1 {
                return Err(reload_failed(
                    "Instance registry may contain at most one default instance",
                )
                .with("field", "default"));
            }
        }
        let single = json!({ "instances": [instance] });
        validate_new_instance(
            instance,
            legacy && has_legacy_instance_secrets(&single),
            env,
        )?;
    }
    let mut out = doc.clone();
    out.insert("instances".into(), Value::Array(normalized));
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// InstanceRegistry

/// Per-path mutation serialization shared by every registry in the process
/// (JS `mutationQueues`).
static MUTATION_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(Default::default);

fn path_lock(path: &Path) -> Arc<Mutex<()>> {
    MUTATION_LOCKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(path.to_path_buf())
        .or_default()
        .clone()
}

#[derive(Clone, Debug, Default)]
pub struct RegistryOptions {
    /// Both set: explicit paths (no precedence resolution).
    pub read_path: Option<PathBuf>,
    pub write_path: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
    pub legacy_path: Option<PathBuf>,
}

/// Default legacy registry. 6.x resolves `<package>/config/servicenow-instances.json`
/// from its own source directory (`instance-registry.js:12`), never from the working
/// directory. The Rust equivalent is the install root of the running binary:
/// `<root>/bin/<exe>` → `<root>/config/servicenow-instances.json`. A cloned project
/// in the working directory can therefore never become the registry.
pub fn default_legacy_path() -> Option<PathBuf> {
    legacy_path_for_exe(&std::env::current_exe().ok()?.canonicalize().ok()?)
}

/// `<exe>/../../config/servicenow-instances.json` (the install root of `exe`).
pub fn legacy_path_for_exe(exe: &Path) -> Option<PathBuf> {
    Some(
        exe.parent()?
            .parent()?
            .join("config")
            .join("servicenow-instances.json"),
    )
}

struct State {
    paths: Option<(PathBuf, PathBuf, ConfigSource)>,
    document: Option<Value>,
    loaded: bool,
    legacy_plaintext: bool,
}

/// Hook run with the next document before a write; an error aborts the write.
pub type PrecommitHook<'a> = &'a mut dyn FnMut(&Value) -> Result<(), CodedError>;
/// Hook observing `(previous, next)` documents before a write.
pub type CaptureHook<'a> = &'a mut dyn FnMut(&Value, &Value) -> Result<(), CodedError>;

/// Options of [`InstanceRegistry::register`].
#[derive(Default)]
pub struct RegisterOptions<'a> {
    pub make_default: bool,
    pub precommit: Option<PrecommitHook<'a>>,
    pub capture_context: Option<CaptureHook<'a>>,
}

pub struct InstanceRegistry {
    options: RegistryOptions,
    env: Env,
    state: Mutex<State>,
}

fn not_found(name: &str) -> CodedError {
    CodedError::new("INSTANCE_NOT_FOUND", format!("Instance '{name}' not found")).with("name", name)
}

fn instances_of(doc: &Value) -> Vec<Value> {
    doc.get("instances")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn name_of(v: &Value) -> Option<&str> {
    v.get("name").and_then(Value::as_str)
}

fn to_object(v: Value) -> Object {
    match v {
        Value::Object(o) => o,
        _ => Object::new(),
    }
}

impl InstanceRegistry {
    /// Paths resolved lazily from `env`/home with the default legacy path.
    pub fn new(env: Env) -> Self {
        Self::with_options(RegistryOptions::default(), env)
    }

    pub fn with_options(options: RegistryOptions, env: Env) -> Self {
        let explicit = match (&options.read_path, &options.write_path) {
            (Some(r), Some(w)) => Some((resolve_path(r), resolve_path(w), ConfigSource::Explicit)),
            _ => None,
        };
        Self {
            options,
            env,
            state: Mutex::new(State {
                paths: explicit,
                document: None,
                loaded: false,
                legacy_plaintext: false,
            }),
        }
    }

    /// Explicit read/write paths (the JS `{ readPath, writePath }` form).
    pub fn at_paths(
        read_path: impl Into<PathBuf>,
        write_path: impl Into<PathBuf>,
        env: Env,
    ) -> Self {
        Self::with_options(
            RegistryOptions {
                read_path: Some(read_path.into()),
                write_path: Some(write_path.into()),
                ..RegistryOptions::default()
            },
            env,
        )
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn resolve_paths(&self, state: &mut State) {
        if state.paths.is_some() {
            return;
        }
        let home = self
            .options
            .home_dir
            .clone()
            .unwrap_or_else(|| home_dir(&self.env));
        let legacy = self
            .options
            .legacy_path
            .clone()
            .or_else(default_legacy_path);
        let p = resolve_config_paths(&self.env, &home, legacy.as_deref(), &|p: &Path| p.exists());
        state.paths = Some((
            resolve_path(&p.read_path),
            resolve_path(&p.write_path),
            p.source,
        ));
    }

    fn paths(&self) -> (PathBuf, PathBuf, ConfigSource) {
        let mut state = self.lock();
        self.resolve_paths(&mut state);
        state
            .paths
            .clone()
            .unwrap_or_else(|| (PathBuf::new(), PathBuf::new(), ConfigSource::User))
    }

    pub fn read_path(&self) -> PathBuf {
        self.paths().0
    }
    pub fn write_path(&self) -> PathBuf {
        self.paths().1
    }
    pub fn source(&self) -> ConfigSource {
        self.paths().2
    }

    pub fn has_file(&self) -> bool {
        self.read_path().exists()
    }

    fn read_document(&self, state: &mut State, file: &Path) -> Result<Value, CodedError> {
        let path_str = file.to_string_lossy().into_owned();
        let document = match std::fs::read_to_string(file) {
            Ok(text) => serde_json::from_str::<Value>(&text).map_err(|_| {
                reload_failed("Failed to parse instance registry JSON")
                    .with("path", path_str.clone())
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                json!({ "version": 1, "instances": [] })
            }
            Err(e) => {
                return Err(
                    reload_failed(format!("Failed to load instance registry: {e}"))
                        .with("path", path_str),
                );
            }
        };
        let validated = validate_document(&document, &self.env).map_err(|e| {
            let mut details =
                if e.code == "REGISTRY_RELOAD_FAILED" || e.code == "INVALID_INSTANCE_CONFIG" {
                    e.details.clone()
                } else {
                    Object::new()
                };
            details.insert("path".into(), path_str.clone().into());
            CodedError {
                code: "REGISTRY_RELOAD_FAILED".into(),
                message: e.message,
                details,
            }
        })?;
        let mut doc = to_object(validated);
        let mut canon = Vec::new();
        for instance in instances_of(&Value::Object(doc.clone())) {
            let mut i = to_object(instance);
            let url = i
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            i.insert("url".into(), canonicalize_instance_url(&url)?.into());
            canon.push(Value::Object(i));
        }
        doc.insert("instances".into(), Value::Array(canon));
        doc.entry("version").or_insert(json!(1));
        let doc = Value::Object(doc);
        state.legacy_plaintext = has_legacy_instance_secrets(&doc);
        state.document = Some(doc.clone());
        state.loaded = true;
        Ok(doc)
    }

    fn raw_locked(&self, state: &mut State) -> Result<Value, CodedError> {
        self.resolve_paths(state);
        if state.loaded
            && let Some(doc) = &state.document
        {
            return Ok(doc.clone());
        }
        let read = state
            .paths
            .as_ref()
            .map(|p| p.0.clone())
            .unwrap_or_default();
        self.read_document(state, &read)
    }

    /// The redacted document (secret-shaped keys removed).
    pub fn load(&self) -> Result<Value, CodedError> {
        let mut state = self.lock();
        Ok(redact_secrets(&self.raw_locked(&mut state)?))
    }

    /// Forces a re-read; a failure restores the previous cache.
    pub fn reload(&self) -> Result<Value, CodedError> {
        let mut state = self.lock();
        let previous = (state.document.clone(), state.loaded, state.legacy_plaintext);
        state.loaded = false;
        match self.raw_locked(&mut state) {
            Ok(doc) => Ok(redact_secrets(&doc)),
            Err(e) => {
                (state.document, state.loaded, state.legacy_plaintext) = previous;
                if e.code == "REGISTRY_RELOAD_FAILED" {
                    return Err(e);
                }
                let read = state
                    .paths
                    .as_ref()
                    .map(|p| p.0.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Err(
                    reload_failed(format!("Failed to reload instance registry: {}", e.message))
                        .with("path", read),
                )
            }
        }
    }

    /// The unredacted cached document (legacy secrets included). Client use only.
    pub fn raw_document(&self) -> Result<Value, CodedError> {
        let mut state = self.lock();
        self.raw_locked(&mut state)
    }

    pub fn list_for_client(&self) -> Result<Vec<Object>, CodedError> {
        Ok(instances_of(&self.raw_document()?)
            .into_iter()
            .map(to_object)
            .collect())
    }

    pub fn get_for_client(&self, name: &str) -> Result<Object, CodedError> {
        instances_of(&self.raw_document()?)
            .into_iter()
            .find(|i| name_of(i) == Some(name))
            .map(to_object)
            .ok_or_else(|| not_found(name))
    }

    pub fn get_default_for_client(&self) -> Result<Option<Object>, CodedError> {
        let items = instances_of(&self.raw_document()?);
        Ok(items
            .iter()
            .find(|i| i.get("default") == Some(&Value::Bool(true)))
            .or_else(|| items.first())
            .cloned()
            .map(to_object))
    }

    pub fn list(&self) -> Result<Vec<Object>, CodedError> {
        Ok(instances_of(&self.load()?)
            .iter()
            .map(|i| to_object(redact_secrets(i)))
            .collect())
    }

    pub fn get(&self, name: &str) -> Result<Object, CodedError> {
        instances_of(&self.load()?)
            .iter()
            .find(|i| name_of(i) == Some(name))
            .map(|i| to_object(redact_secrets(i)))
            .ok_or_else(|| not_found(name))
    }

    pub fn get_default(&self) -> Result<Option<Object>, CodedError> {
        let items = instances_of(&self.load()?);
        Ok(items
            .iter()
            .find(|i| i.get("default") == Some(&Value::Bool(true)))
            .or_else(|| items.first())
            .map(|i| to_object(redact_secrets(i))))
    }

    /// Lenient validation (legacy plaintext secrets admitted).
    pub fn validate(&self, instance: &Value) -> Result<(), CodedError> {
        validate_new_instance(instance, true, &self.env)
    }

    pub fn validate_document(&self, document: &Value) -> Result<Value, CodedError> {
        validate_document(document, &self.env)
    }

    pub fn register(
        &self,
        instance: &Value,
        mut options: RegisterOptions<'_>,
    ) -> Result<Object, CodedError> {
        let requested = name_of(instance).unwrap_or("").to_owned();
        let precommit = options.precommit.take();
        let capture = options.capture_context.take();
        let make_default = options.make_default;
        let env = self.env.clone();
        self.mutate(
            |doc| {
                let candidate = normalize_instance_credential_refs(instance, false)?;
                validate_new_instance(&candidate, false, &env)?;
                let current = instances_of(doc);
                let name = name_of(&candidate).unwrap_or("").to_owned();
                if current.iter().any(|i| name_of(i) == Some(&name)) {
                    return Err(CodedError::new(
                        "INSTANCE_ALREADY_EXISTS",
                        format!("Instance '{name}' already exists"),
                    )
                    .with("name", name));
                }
                let should_default = current.is_empty()
                    || make_default
                    || candidate.get("default") == Some(&Value::Bool(true));
                let mut c = to_object(candidate);
                let url = c
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                c.insert("url".into(), canonicalize_instance_url(&url)?.into());
                c.entry("authType").or_insert("basic".into());
                c.insert("default".into(), should_default.into());
                let mut items: Vec<Value> = current
                    .into_iter()
                    .map(|mut i| {
                        if should_default && let Some(o) = i.as_object_mut() {
                            o.insert("default".into(), false.into());
                        }
                        i
                    })
                    .collect();
                items.push(Value::Object(c));
                Ok(with_instances(doc, items))
            },
            |next| find(next, &requested),
            precommit,
            capture,
        )
        .map(|v| to_object(v.unwrap_or(Value::Null)))
    }

    pub fn update(&self, name: &str, patch: &Object) -> Result<Object, CodedError> {
        let env = self.env.clone();
        self.mutate(
            |doc| {
                let current = find(doc, name).ok_or_else(|| not_found(name))?;
                let mut merged = to_object(current);
                for (k, v) in patch {
                    merged.insert(k.clone(), v.clone());
                }
                merged.insert("name".into(), name.into());
                let merged = normalize_instance_credential_refs(&Value::Object(merged), false)?;
                validate_new_instance(&merged, false, &env)?;
                let mut merged = to_object(merged);
                let url = merged
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                merged.insert("url".into(), canonicalize_instance_url(&url)?.into());
                let should_default = merged.get("default") == Some(&Value::Bool(true));
                merged.insert("default".into(), should_default.into());
                let items = instances_of(doc)
                    .into_iter()
                    .map(|mut i| {
                        if name_of(&i) == Some(name) {
                            return Value::Object(merged.clone());
                        }
                        if should_default && let Some(o) = i.as_object_mut() {
                            o.insert("default".into(), false.into());
                        }
                        i
                    })
                    .collect();
                Ok(with_instances(doc, items))
            },
            |next| find(next, name),
            None,
            None,
        )
        .map(|v| to_object(v.unwrap_or(Value::Null)))
    }

    /// Undo a registration if the entry is still exactly `expected`.
    pub fn compensate_registration(
        &self,
        name: &str,
        expected: Option<&Value>,
        prior_default: Option<(&str, bool)>,
    ) -> Result<(), CodedError> {
        let env = self.env.clone();
        self.mutate(
            |doc| {
                let current = find(doc, name);
                if current.is_none() || expected.is_none() || current.as_ref() != expected {
                    return Err(CodedError::new(
                        "REGISTRY_ROLLBACK_REQUIRED",
                        format!("Instance '{name}' changed before registration compensation"),
                    )
                    .with("name", name)
                    .with("rollbackRequired", true));
                }
                let items = instances_of(doc)
                    .into_iter()
                    .filter(|i| name_of(i) != Some(name))
                    .map(|mut i| {
                        if let Some((prior, was_default)) = prior_default
                            && name_of(&i) == Some(prior)
                            && let Some(o) = i.as_object_mut()
                        {
                            o.insert("default".into(), was_default.into());
                        }
                        i
                    })
                    .collect();
                let next = with_instances(doc, items);
                validate_document(&next, &env)?;
                Ok(next)
            },
            |_| None,
            None,
            None,
        )
        .map(|_| ())
    }

    pub fn remove(&self, name: &str, expected: Option<&Value>) -> Result<(), CodedError> {
        self.mutate(
            |doc| {
                let current = find(doc, name).ok_or_else(|| not_found(name))?;
                if expected.is_some_and(|e| *e != current) {
                    return Err(CodedError::new(
                        "INSTANCE_CHANGED",
                        format!("Instance '{name}' changed before compensation"),
                    )
                    .with("name", name));
                }
                let items = instances_of(doc)
                    .into_iter()
                    .filter(|i| name_of(i) != Some(name))
                    .collect();
                Ok(with_instances(doc, items))
            },
            |_| None,
            None,
            None,
        )
        .map(|_| ())
    }

    fn mutate(
        &self,
        operation: impl FnOnce(&Value) -> Result<Value, CodedError>,
        select: impl FnOnce(&Value) -> Option<Value>,
        precommit: Option<PrecommitHook<'_>>,
        capture: Option<CaptureHook<'_>>,
    ) -> Result<Option<Value>, CodedError> {
        let (read, write, _) = self.paths();
        let lock = path_lock(&write);
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        // The per-path write lock serializes mutations; the registry state lock
        // is held only to read the current document and to commit, so slow
        // hooks (credential checks, keychain probes) never stall readers.
        let current = {
            let mut state = self.lock();
            let source = if write.exists() {
                write.clone()
            } else {
                read.clone()
            };
            let current = self.read_document(&mut state, &source)?;
            if state.legacy_plaintext {
                return Err(CodedError::new(
                    "LEGACY_MIGRATION_REQUIRED",
                    "This registry contains plaintext credentials; migrate it before changing instances",
                )
                .with("path", read.to_string_lossy().into_owned()));
            }
            current
        };
        let next = operation(&current)?;
        validate_document(&next, &self.env)?;
        if let Some(capture) = capture {
            capture(&current, &next).map_err(|_| {
                CodedError::new(
                    "REGISTRY_CONTEXT_FAILED",
                    "Registry mutation context could not be captured",
                )
            })?;
        }
        if let Some(precommit) = precommit {
            precommit(&next)?;
        }
        write_atomic(&write, &next)?;
        self.commit(&mut self.lock(), next.clone(), &write);
        Ok(select(&next))
    }

    fn commit(&self, state: &mut State, document: Value, write: &Path) {
        state.document = Some(document);
        state.loaded = true;
        state.legacy_plaintext = false;
        if let Some(paths) = state.paths.as_mut()
            && paths.0 != paths.1
        {
            paths.0 = write.to_path_buf();
            paths.2 = ConfigSource::User;
        }
    }

    /// Writes a migrated document to the write path (used by `instance migrate`).
    pub fn write_migrated_document(&self, document: &Value) -> Result<(), CodedError> {
        let (_, write, _) = self.paths();
        let lock = path_lock(&write);
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        write_atomic(&write, document)?;
        let mut state = self.lock();
        state.document = Some(document.clone());
        state.loaded = true;
        state.legacy_plaintext = false;
        if let Some(paths) = state.paths.as_mut() {
            paths.0 = write.clone();
            paths.2 = ConfigSource::User;
        }
        Ok(())
    }
}

fn find(doc: &Value, name: &str) -> Option<Value> {
    instances_of(doc)
        .into_iter()
        .find(|i| name_of(i) == Some(name))
}

fn with_instances(doc: &Value, items: Vec<Value>) -> Value {
    let mut out = doc.as_object().cloned().unwrap_or_default();
    out.insert("version".into(), json!(1));
    out.insert("instances".into(), Value::Array(items));
    Value::Object(out)
}

/// Temp file (`wx`, 0600) in a 0700 directory, then rename over the target.
fn write_atomic(target: &Path, document: &Value) -> Result<(), CodedError> {
    let fail = || {
        CodedError::new("REGISTRY_WRITE_FAILED", "Failed to write instance registry")
            .with("path", resolve_path(target).to_string_lossy().into_owned())
    };
    let directory = target.parent().ok_or_else(fail)?;
    let base = target
        .file_name()
        .ok_or_else(fail)?
        .to_string_lossy()
        .into_owned();
    create_private_dir(directory).map_err(|_| fail())?;
    let text = serde_json::to_string_pretty(document).map_err(|_| fail())? + "\n";
    let mut temp = None;
    for attempt in 0..10 {
        let candidate = directory.join(format!(
            ".{base}.{}.{}.{attempt}.tmp",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&candidate) {
            Ok(file) => {
                temp = Some((candidate, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 9 => {}
            Err(_) => return Err(fail()),
        }
    }
    let (temp_path, mut file) = temp.ok_or_else(fail)?;
    let result = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            std::fs::rename(&temp_path, target)
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(fail());
    }
    Ok(())
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
