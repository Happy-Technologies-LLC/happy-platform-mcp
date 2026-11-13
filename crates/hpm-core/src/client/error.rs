//! Client error type and credential redaction (ports the sanitizers in
//! `src/servicenow-client.js`: `safeCredentialError`, `safeAuthError`,
//! `sanitizeServiceNowError`, `credentialSafeErrorSummary`).
//!
//! A [`ClientError`] only ever carries redacted text, a string code, an HTTP
//! status and a sanitized copy of the response body. Request config, headers
//! and transport internals are never retained, so nothing can echo a secret.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

pub const STALE_INSTANCE_CODE: &str = "INSTANCE_STALE_DURING_REQUEST";

const CREDENTIAL_ERROR_CODES: [&str; 4] = [
    "CREDENTIAL_NOT_FOUND",
    "INVALID_CREDENTIAL_REF",
    "KEYCHAIN_UNAVAILABLE",
    "KEYCHAIN_OPERATION_FAILED",
];

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ClientError {
    pub name: String,
    pub message: String,
    pub code: Option<String>,
    pub status: Option<u16>,
    /// Sanitized response body (credential fields removed, strings redacted).
    pub response_data: Option<Value>,
    /// OAuth policy field (`authorizeUrl` / `tokenUrl`).
    pub field: Option<String>,
    /// Script-processor reason / cicd detail.
    pub reason: Option<String>,
}

impl ClientError {
    pub fn new(name: &str, message: impl Into<String>, code: Option<&str>) -> Self {
        Self {
            name: name.to_owned(),
            message: message.into(),
            code: code.map(str::to_owned),
            status: None,
            response_data: None,
            field: None,
            reason: None,
        }
    }

    pub fn plain(message: impl Into<String>) -> Self {
        Self::new("Error", message, None)
    }

    pub fn with_status(mut self, status: Option<u16>) -> Self {
        self.status = status;
        self
    }

    /// `StaleInstanceError`.
    pub fn stale() -> Self {
        Self::new(
            "StaleInstanceError",
            "Instance changed during credential resolution",
            Some(STALE_INSTANCE_CODE),
        )
    }

    pub fn is_stale(&self) -> bool {
        self.code.as_deref() == Some(STALE_INSTANCE_CODE)
    }

    /// `AuthenticationError`.
    pub fn auth(message: impl Into<String>, code: &str) -> Self {
        Self::new("AuthenticationError", message, Some(code))
    }

    /// `safeCredentialError`: fixed messages, never the reference or backend text.
    pub fn credential(code: &str) -> Self {
        let code = if CREDENTIAL_ERROR_CODES.contains(&code) {
            code
        } else {
            "KEYCHAIN_OPERATION_FAILED"
        };
        let message = match code {
            "CREDENTIAL_NOT_FOUND" => "Credential not found",
            "INVALID_CREDENTIAL_REF" => "Invalid credential reference",
            "KEYCHAIN_UNAVAILABLE" => "Credential store unavailable",
            _ => "Credential store operation failed",
        };
        Self::new("CredentialResolutionError", message, Some(code))
    }

    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// `JSON.stringify(error)` view.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("name".into(), Value::String(self.name.clone()));
        out.insert("message".into(), Value::String(self.message.clone()));
        if let Some(code) = &self.code {
            out.insert("code".into(), Value::String(code.clone()));
        }
        if let Some(status) = self.status {
            out.insert("status".into(), Value::from(status));
        }
        if let Some(field) = &self.field {
            out.insert("field".into(), Value::String(field.clone()));
        }
        if let Some(reason) = &self.reason {
            // `CicdApiError` names this field `detail`.
            let key = if self.name == "CicdApiError" {
                "detail"
            } else {
                "reason"
            };
            out.insert(key.into(), Value::String(reason.clone()));
        }
        if let Some(data) = &self.response_data {
            let mut response = Map::new();
            if let Some(status) = self.status {
                response.insert("status".into(), Value::from(status));
            }
            response.insert("data".into(), data.clone());
            out.insert("response".into(), Value::Object(response));
        }
        Value::Object(out)
    }
}

pub fn is_credential_error_code(code: &str) -> bool {
    CREDENTIAL_ERROR_CODES.contains(&code)
}

/// Set of secret strings to scrub, longest first.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    values: BTreeSet<String>,
}

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, value: &str) {
        if !value.is_empty() {
            self.values.insert(value.to_owned());
        }
    }

    /// `addAuthorizationVariants`.
    pub fn add_authorization_variants(&mut self, scheme: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        for variant in [scheme.to_owned(), scheme.to_lowercase()] {
            self.add(&format!("{variant} {value}"));
            self.add(&format!("Authorization: {variant} {value}"));
            self.add(&format!("authorization: {variant} {value}"));
        }
    }

    /// `addAuthorizationValue`: the value plus its bare token and scheme variants.
    pub fn add_authorization_value(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        self.add(value);
        let mut rest = value;
        if let Some(idx) = find_ci(rest, "authorization") {
            if idx == 0 {
                let after = rest["authorization".len()..].trim_start();
                if let Some(stripped) = after.strip_prefix(':') {
                    rest = stripped.trim_start();
                }
            }
        }
        for scheme in ["basic", "bearer"] {
            if rest.len() > scheme.len() && rest[..scheme.len()].eq_ignore_ascii_case(scheme) {
                let after = &rest[scheme.len()..];
                let token = after.trim_start();
                if token.len() < after.len() && !token.is_empty() {
                    let original = &rest[..scheme.len()];
                    self.add(token);
                    self.add(&format!("{original} {token}"));
                    self.add(&format!("{} {token}", original.to_lowercase()));
                }
            }
        }
    }

    /// `collectErrorCredentialValues` over a response body: values under
    /// credential-named keys, and `code` inside form-encoded bodies.
    pub fn collect_from_value(&mut self, value: &Value, key: &str) {
        match value {
            Value::String(s) => {
                if is_credential_field(key) {
                    self.add_authorization_value(s);
                } else if key.eq_ignore_ascii_case("data") || key.eq_ignore_ascii_case("body") {
                    if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                        self.collect_from_value(&parsed, key);
                    } else {
                        for (field, field_value) in url::form_urlencoded::parse(s.as_bytes()) {
                            if field == "code" && !field_value.is_empty() {
                                self.add(&field_value);
                            }
                            self.collect_from_value(
                                &Value::String(field_value.into_owned()),
                                &field,
                            );
                        }
                    }
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.collect_from_value(item, &i.to_string());
                }
            }
            Value::Object(map) => {
                for (k, v) in map {
                    self.collect_from_value(v, k);
                }
            }
            _ => {}
        }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut values: Vec<&String> = self.values.iter().collect();
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        let mut out = text.to_owned();
        for secret in values {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "[redacted]");
            }
        }
        out
    }

    /// `sanitizeCopy`: drops credential/header keys, redacts every string.
    pub fn sanitize(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.redact(s)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.sanitize(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(k, _)| !is_redacted_field(k))
                    .map(|(k, v)| (k.clone(), self.sanitize(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack.to_ascii_lowercase().find(needle)
}

pub fn is_credential_field(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "authorization"
            | "access_token"
            | "refresh_token"
            | "client_secret"
            | "code_verifier"
            | "password"
            | "token"
            | "id_token"
            | "basic"
    ) || lower.contains("authorization")
}

fn is_redacted_field(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    is_credential_field(key) || lower == "header" || lower == "headers"
}

/// `enrichServiceNowError` message: `"<status> <message> <detail>"` from a
/// Table API error body, `"<status>: <text>"` for a text body, else `fallback`.
pub fn enrich_message(status: Option<u16>, data: &Value, fallback: &str) -> String {
    if let Some(error) = data.get("error") {
        let msg = error.get("message").and_then(Value::as_str).unwrap_or("");
        let detail = error.get("detail").and_then(Value::as_str).unwrap_or("");
        if !msg.is_empty() || !detail.is_empty() {
            let mut parts = Vec::new();
            if let Some(status) = status.filter(|s| *s != 0) {
                parts.push(status.to_string());
            }
            for part in [msg, detail] {
                if !part.is_empty() {
                    parts.push(part.to_owned());
                }
            }
            return parts.join(" ");
        }
    }
    if let Value::String(text) = data {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return match status.filter(|s| *s != 0) {
                Some(status) => format!("{status}: {trimmed}"),
                None => trimmed.to_owned(),
            };
        }
    }
    fallback.to_owned()
}

/// A raw failure from an HTTP exchange or an injected seam (token POST,
/// interactive flow). Its fields are untrusted and may contain secrets; it is
/// only ever turned into a [`ClientError`] through a [`Redactor`].
#[derive(Clone, Debug, Default)]
pub struct RawError {
    pub name: Option<String>,
    pub message: String,
    pub code: Option<String>,
    pub status: Option<u16>,
    pub data: Option<Value>,
}

impl RawError {
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    pub fn http(status: u16, data: Value) -> Self {
        Self {
            message: format!("Request failed with status code {status}"),
            status: Some(status),
            data: Some(data),
            ..Self::default()
        }
    }

    /// An already-safe client error carried through a seam.
    pub fn from_client(error: &ClientError) -> Self {
        Self {
            name: Some(error.name.clone()),
            message: error.message.clone(),
            code: error.code.clone(),
            status: error.status,
            data: None,
        }
    }

    /// `isExplicitInvalidGrant`.
    pub fn is_explicit_invalid_grant(&self) -> bool {
        let is = |v: Option<&Value>| {
            v.and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("invalid_grant"))
        };
        let data = self.data.as_ref();
        is(data)
            || is(data.and_then(|d| d.get("error")))
            || is(data
                .and_then(|d| d.get("error"))
                .and_then(|e| e.get("code")))
            || is(data.and_then(|d| d.get("error_code")))
            || is(data.and_then(|d| d.get("code")))
            || self
                .code
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case("invalid_grant"))
    }

    /// `safeAuthError`.
    pub fn into_safe_auth(self, mut redactor: Redactor) -> ClientError {
        if let Some(data) = &self.data {
            redactor.collect_from_value(data, "data");
            redactor.collect_from_value(data, "");
        }
        let message = if self.message.is_empty() {
            "Authentication request failed".to_owned()
        } else {
            self.message.clone()
        };
        let name = if self.name.as_deref() == Some("AuthenticationError") {
            "AuthenticationError"
        } else {
            "Error"
        };
        let mut safe = ClientError::new(name, redactor.redact(&message), None);
        safe.status = self.status;
        safe.code = match self.status {
            Some(401) => Some("AUTHENTICATION_FAILED".into()),
            Some(403) => Some("AUTHORIZATION_FAILED".into()),
            _ => self.code.clone().filter(|c| !is_credential_error_code(c)),
        };
        safe.response_data = self.data.as_ref().map(|d| redactor.sanitize(d));
        safe
    }

    /// `sanitizeServiceNowError` after `enrichServiceNowError`.
    pub fn into_safe_servicenow(self, mut redactor: Redactor) -> ClientError {
        if let Some(data) = &self.data {
            redactor.collect_from_value(data, "");
        }
        let fallback = if self.message.is_empty() {
            "ServiceNow API request failed"
        } else {
            self.message.as_str()
        };
        let message = match &self.data {
            Some(data) => enrich_message(self.status, data, fallback),
            None => fallback.to_owned(),
        };
        let mut safe = ClientError::new(
            &redactor.redact(self.name.as_deref().unwrap_or("Error")),
            redactor.redact(&message),
            None,
        );
        safe.code = self.code;
        safe.status = self.status;
        safe.response_data = self.data.as_ref().map(|d| redactor.sanitize(d));
        safe
    }

    /// `credentialSafeErrorSummary`: redacted message, code and status only.
    pub fn into_summary(self, mut redactor: Redactor) -> ClientError {
        if let Some(data) = &self.data {
            redactor.collect_from_value(data, "");
        }
        let message = if self.message.is_empty() {
            "Request failed".to_owned()
        } else {
            self.message
        };
        let mut safe = ClientError::new("Error", redactor.redact(&message), None);
        safe.code = self.code;
        safe.status = self.status;
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// servicenow-client-errors.test.js: enrichServiceNowError message.
    #[test]
    fn surfaces_message_and_detail_preserving_data() {
        let data = json!({"status":"failure","error":{"message":"Table access was denied.","detail":"The table is not available through web services."}});
        let safe = RawError::http(403, data.clone()).into_safe_servicenow(Redactor::new());
        assert_eq!(
            safe.message,
            "403 Table access was denied. The table is not available through web services."
        );
        assert_eq!(safe.response_data, Some(data));
    }

    /// 'returns a safe cycle-free copy': credential fields dropped, diagnostics kept.
    #[test]
    fn sanitizes_basic_credentials_in_bodies() {
        let password = "frozen-password-secret";
        let auth = format!(
            "Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                format!("user:{password}")
            )
        );
        let mut redactor = Redactor::new();
        redactor.add(password);
        redactor.add_authorization_value(&auth);
        let mut raw = RawError::http(
            502,
            json!({"error":{"message":format!("Authorization {auth}"),"password":password,"detail":"retain this diagnostic"}}),
        );
        raw.message = format!("request failed with {password}");
        raw.code = Some("ERR_BAD_RESPONSE".into());
        let safe = raw.into_safe_servicenow(redactor);
        assert_eq!(safe.code.as_deref(), Some("ERR_BAD_RESPONSE"));
        assert_eq!(safe.status, Some(502));
        let data = safe.response_data.clone().unwrap_or_default();
        assert!(data["error"].get("password").is_none());
        assert_eq!(data["error"]["detail"], "retain this diagnostic");
        let serialized = safe.to_json().to_string();
        assert!(!serialized.contains(password));
        assert!(!serialized.contains(&auth));
    }

    #[test]
    fn authorization_value_variants() {
        let mut r = Redactor::new();
        r.add_authorization_value("Authorization: Bearer tok123");
        assert_eq!(r.redact("x tok123 y"), "x [redacted] y");
        assert_eq!(r.redact("bearer tok123"), "[redacted]");
    }

    #[test]
    fn credential_error_messages() {
        assert_eq!(
            ClientError::credential("CREDENTIAL_NOT_FOUND").message,
            "Credential not found"
        );
        assert_eq!(
            ClientError::credential("weird").code(),
            Some("KEYCHAIN_OPERATION_FAILED")
        );
        let stale = ClientError::stale();
        assert_eq!(stale.name, "StaleInstanceError");
        assert_eq!(stale.code(), Some(STALE_INSTANCE_CODE));
    }
}
