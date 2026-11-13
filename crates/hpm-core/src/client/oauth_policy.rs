//! OAuth endpoint origin policy (ports `src/oauth-endpoint-policy.js`).
//!
//! Endpoints default to the instance origin; an external IdP origin is accepted
//! only when the local operator lists it in [`TRUSTED_OAUTH_ORIGINS_ENV`]. The
//! allow-list comes from the process environment only, re-read on every check.

use std::collections::BTreeSet;

use url::Url;

use super::error::ClientError;

pub const TRUSTED_OAUTH_ORIGINS_ENV: &str = "SERVICENOW_OAUTH_TRUSTED_ORIGINS";
pub const MAX_URL_LENGTH: usize = 4096;

const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

fn policy_error(code: &str, message: String, field: Option<&str>) -> ClientError {
    let mut e = ClientError::new("OAuthEndpointPolicyError", message, Some(code));
    e.field = field.map(str::to_owned);
    e
}

fn host_of(url: &Url) -> String {
    match url.host() {
        Some(url::Host::Ipv6(addr)) => format!("[{addr}]"),
        Some(host) => host.to_string(),
        None => String::new(),
    }
}

fn is_allowed_scheme(url: &Url) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http" && LOOPBACK_HOSTS.contains(&host_of(url).as_str()))
}

pub fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// JS length is UTF-16 units; ASCII URLs make chars a faithful bound.
fn exceeds_url_length(value: &str) -> bool {
    value.encode_utf16().count() > MAX_URL_LENGTH
}

pub fn trim_trailing_slashes(value: &str) -> &str {
    value.trim_end_matches('/')
}

fn invalid_trusted_origins(position: usize) -> ClientError {
    policy_error(
        "OAUTH_TRUSTED_ORIGINS_INVALID",
        format!(
            "{TRUSTED_OAUTH_ORIGINS_ENV} entry {position} must be an exact https:// origin (or loopback http://) with no path, query, fragment, credentials, or wildcard"
        ),
        None,
    )
}

/// `parseTrustedOAuthOrigins`.
pub fn parse_trusted_oauth_origins(value: Option<&str>) -> Result<BTreeSet<String>, ClientError> {
    let mut origins = BTreeSet::new();
    let Some(value) = value else {
        return Ok(origins);
    };
    for (index, raw) in value.split(',').enumerate() {
        let entry = raw.trim();
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
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !is_allowed_scheme(&parsed)
        {
            return Err(invalid_trusted_origins(index + 1));
        }
        origins.insert(origin_of(&parsed));
    }
    Ok(origins)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthEndpoints {
    pub authorize_url: String,
    pub token_url: String,
}

/// `resolveOAuthEndpoints`.
pub fn resolve_oauth_endpoints(
    instance_url: &str,
    authorize_url: Option<&str>,
    token_url: Option<&str>,
) -> OAuthEndpoints {
    OAuthEndpoints {
        authorize_url: authorize_url
            .filter(|s| !s.is_empty())
            .map_or_else(|| format!("{instance_url}/oauth_auth.do"), str::to_owned),
        token_url: token_url
            .filter(|s| !s.is_empty())
            .map_or_else(|| format!("{instance_url}/oauth_token.do"), str::to_owned),
    }
}

/// `assertApprovedOAuthEndpoint`; `trusted_env` is the current value of
/// [`TRUSTED_OAUTH_ORIGINS_ENV`]. Returns the normalized endpoint (`href`).
pub fn assert_approved_oauth_endpoint(
    endpoint: &str,
    instance_url: &str,
    field: &str,
    trusted_env: Option<&str>,
) -> Result<String, ClientError> {
    let trusted = parse_trusted_oauth_origins(trusted_env)?;
    if exceeds_url_length(endpoint) || exceeds_url_length(instance_url) {
        return Err(policy_error(
            "OAUTH_ENDPOINT_NOT_APPROVED",
            format!("{field} and the instance URL must be at most {MAX_URL_LENGTH} characters"),
            Some(field),
        ));
    }
    let (Ok(parsed), Ok(instance)) = (Url::parse(endpoint), Url::parse(instance_url)) else {
        return Err(policy_error(
            "OAUTH_ENDPOINT_NOT_APPROVED",
            format!("{field} must be a valid URL"),
            Some(field),
        ));
    };
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some_and(|f| !f.is_empty())
        || !is_allowed_scheme(&parsed)
    {
        return Err(policy_error(
            "OAUTH_ENDPOINT_NOT_APPROVED",
            format!(
                "{field} must be an HTTPS URL (or loopback HTTP) without credentials or fragment"
            ),
            Some(field),
        ));
    }
    let origin = origin_of(&parsed);
    if origin == origin_of(&instance) || trusted.contains(&origin) {
        return Ok(parsed.as_str().to_owned());
    }
    Err(policy_error(
        "OAUTH_ENDPOINT_NOT_APPROVED",
        format!(
            "{field} origin {origin} is not the instance origin and is not listed in {TRUSTED_OAUTH_ORIGINS_ENV}"
        ),
        Some(field),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// oauth-identity-binding.test.js: malformed or wildcard allow-lists fail.
    #[test]
    fn rejects_malformed_or_wildcard_trusted_origins() {
        for value in [
            "*",
            "https://idp.example/path",
            "http://idp.example",
            "https://user:pw@idp.example",
            "not a url",
        ] {
            let err = assert_approved_oauth_endpoint(
                "https://idp.example/token",
                "http://127.0.0.1:1",
                "tokenUrl",
                Some(value),
            )
            .err()
            .map(|e| e.code);
            assert_eq!(
                err,
                Some(Some("OAUTH_TRUSTED_ORIGINS_INVALID".into())),
                "{value}"
            );
        }
    }

    #[test]
    fn approves_same_origin_and_listed_origins_only() {
        let inst = "https://ex.service-now.com";
        assert_eq!(
            assert_approved_oauth_endpoint(
                "https://ex.service-now.com/oauth_token.do",
                inst,
                "tokenUrl",
                None
            )
            .ok(),
            Some("https://ex.service-now.com/oauth_token.do".into())
        );
        let denied =
            assert_approved_oauth_endpoint("https://evil.example/token", inst, "tokenUrl", None);
        assert_eq!(
            denied.err().and_then(|e| e.code),
            Some("OAUTH_ENDPOINT_NOT_APPROVED".into())
        );
        assert!(
            assert_approved_oauth_endpoint(
                "https://idp.example/token",
                inst,
                "tokenUrl",
                Some("https://unrelated.example, https://idp.example")
            )
            .is_ok()
        );
        assert!(
            assert_approved_oauth_endpoint(
                "http://evil.example/t",
                "http://evil.example",
                "tokenUrl",
                None
            )
            .is_err()
        );
        assert!(
            assert_approved_oauth_endpoint(
                "http://127.0.0.1:9/t",
                "http://127.0.0.1:9",
                "tokenUrl",
                None
            )
            .is_ok()
        );
    }
}
