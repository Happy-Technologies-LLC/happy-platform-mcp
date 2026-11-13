//! S3 packaging spike: byte-exact ports of the JS keychain identifiers.
//!
//! * service / credential account names: `src/instance-credential-store.js:3-6,45-48`
//! * refresh-token account `identity-v1-{sha256(JSON tuple)}`: `src/servicenow-client.js:529-555`

use sha2::{Digest, Sha256};
use url::Url;

/// Keychain service for every item (`src/instance-credential-store.js:3`, `src/token-store.js:21`).
pub const SERVICE_NAME: &str = "happy-platform-mcp";
/// `src/instance-credential-store.js:4`.
pub const CREDENTIAL_PREFIX: &str = "keychain:instance/";
/// `src/servicenow-client.js:529`.
pub const REFRESH_TOKEN_IDENTITY_PREFIX: &str = "identity-v1-";
/// First element of the hashed tuple (`src/servicenow-client.js:546`).
pub const REFRESH_TOKEN_TUPLE_TAG: &str = "happy-platform-mcp/oauth-refresh-token";

/// `keychain:instance/{name}/{type}` where type is `password` or `client-secret`.
pub fn credential_ref(instance_name: &str, kind: &str) -> String {
    format!("{CREDENTIAL_PREFIX}{instance_name}/{kind}")
}

/// Port of `canonicalIdentityInstanceUrl` (`src/servicenow-client.js:531-534`):
/// `${origin}${pathname without trailing '/'}`.
pub fn canonical_instance_url(url: &str) -> Result<String, url::ParseError> {
    let parsed = Url::parse(url)?;
    let path = parsed.path().trim_end_matches('/');
    Ok(format!("{}{}", parsed.origin().ascii_serialization(), path))
}

/// Port of `oauthRefreshTokenAccount` (`src/servicenow-client.js:542-555`).
///
/// `authorize_url` / `token_url` follow JS `||` semantics: `None` *or* an empty
/// string falls back to `{instance}/oauth_auth.do` / `{instance}/oauth_token.do`
/// (`src/oauth-endpoint-policy.js:81-86`). The tuple is serialized with compact
/// JSON, which matches `JSON.stringify` for strings and integers (both escape
/// only `"`, `\`, and U+0000..U+001F, with identical short escapes and
/// lowercase `\u00xx`; non-ASCII is emitted raw).
pub fn identity_v1_account(
    instance_url: &str,
    client_id: &str,
    authorize_url: Option<&str>,
    token_url: Option<&str>,
    os_user: &str,
) -> Result<String, url::ParseError> {
    let tuple = identity_v1_tuple_json(instance_url, client_id, authorize_url, token_url, os_user)?;
    let digest = Sha256::digest(tuple.as_bytes());
    Ok(format!("{REFRESH_TOKEN_IDENTITY_PREFIX}{}", hex(&digest)))
}

/// The exact UTF-8 string that is hashed: `JSON.stringify(tuple)` in JS.
pub fn identity_v1_tuple_json(
    instance_url: &str,
    client_id: &str,
    authorize_url: Option<&str>,
    token_url: Option<&str>,
    os_user: &str,
) -> Result<String, url::ParseError> {
    let instance = canonical_instance_url(instance_url)?;
    let authorize = non_empty(authorize_url)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{instance}/oauth_auth.do"));
    let token = non_empty(token_url)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{instance}/oauth_token.do"));
    Ok(serde_json::json!([
        REFRESH_TOKEN_TUPLE_TAG,
        1,
        os_user,
        instance,
        client_id,
        Url::parse(&authorize)?.as_str(),
        Url::parse(&token)?.as_str(),
    ])
    .to_string())
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Target triple facts baked in at compile time (used by `--s3-info`).
pub fn build_info() -> serde_json::Value {
    serde_json::json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
    })
}
