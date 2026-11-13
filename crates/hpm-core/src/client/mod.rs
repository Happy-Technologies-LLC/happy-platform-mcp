//! ServiceNow client core (ports `ServiceNowClient` of `src/servicenow-client.js`):
//! instance generations, Basic and OAuth auth (password, client_credentials,
//! refresh and authorization_code + PKCE), keychain credential references,
//! the Table API, the UI pickers and `/sys.scripts.do`.
//!
//! Every await re-checks the instance generation: a `set_instance` in between
//! fails the call with `INSTANCE_STALE_DURING_REQUEST` instead of mixing two
//! instances' credentials or responses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hpm_types::BoxFut;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::credential::{InstanceCredentialStore, parse_credential_ref};
use crate::keychain::{SERVICE_NAME, SecretStore, SecretStoreError};

pub mod error;
mod flight;
pub mod http;
pub mod oauth_code;
pub mod oauth_policy;
mod pickers;
pub mod script_processor;
mod scripts;
#[cfg(test)]
mod tests;

pub use error::{ClientError, RawError, Redactor};
use flight::Flights;
use http::{Clock, HttpRequest, HttpResponse, HttpTransport, Method, form_encode};
use oauth_code::{AuthCodeFlow, AuthCodeFlowConfig, HttpTokenPoster, TokenPoster};
use oauth_policy::{
    MAX_URL_LENGTH, TRUSTED_OAUTH_ORIGINS_ENV, assert_approved_oauth_endpoint,
    resolve_oauth_endpoints, trim_trailing_slashes,
};
pub use scripts::{ProcessorOptions, TriggerOptions};

pub const REST_USER_AGENT: &str = "axios/1.20.0";
pub const UI_USER_AGENT: &str = "ServiceNow-MCP-Client/2.0";
const REFRESH_TOKEN_IDENTITY_PREFIX: &str = "identity-v1-";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthType {
    #[default]
    Basic,
    OAuth,
}

impl AuthType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::OAuth => "oauth",
        }
    }
}

/// `setInstance` arguments. `password`/`client_secret` are legacy inline
/// secrets; `credential_ref` is the 6.x keychain reference (a string, or an
/// object with `password`/`clientSecret`/`passwordRef`/`clientSecretRef`).
#[derive(Clone, Debug, Default)]
pub struct InstanceConfig {
    pub url: String,
    pub name: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub auth_type: AuthType,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub grant_type: Option<String>,
    pub scope: Option<String>,
    pub authorize_url: Option<String>,
    pub token_url: Option<String>,
    pub redirect_port: Option<u16>,
    pub callback_path: Option<String>,
    pub credential_ref: Option<Value>,
}

/// Injected seams. Production wires the OS keychain, reqwest and the loopback
/// browser flow; tests inject fakes.
#[derive(Clone)]
pub struct ClientDeps {
    pub transport: Arc<dyn HttpTransport>,
    pub clock: Arc<dyn Clock>,
    pub credential_store: InstanceCredentialStore,
    /// OAuth refresh tokens (`getRefreshToken`/`setRefreshToken`/`clearRefreshToken`).
    pub token_store: Arc<dyn SecretStore>,
    pub auth_flow: Arc<dyn AuthCodeFlow>,
    /// Token-endpoint POST; defaults to a no-redirect form POST over `transport`.
    pub token_poster: Option<Arc<dyn TokenPoster>>,
    pub os_user: String,
    /// Current value of [`TRUSTED_OAUTH_ORIGINS_ENV`], read at every check.
    pub trusted_origins: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl ClientDeps {
    pub fn env_trusted_origins() -> Arc<dyn Fn() -> Option<String> + Send + Sync> {
        Arc::new(|| std::env::var(TRUSTED_OAUTH_ORIGINS_ENV).ok())
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OAuthCache {
    pub token: Option<String>,
    pub refresh_token: Option<String>,
    pub expiry: Option<i64>,
}

pub(crate) struct State {
    pub generation: u64,
    pub url: String,
    pub name: String,
    pub cfg: InstanceConfig,
    /// base64(`user:password`) for a legacy inline Basic password.
    pub basic_auth: Option<String>,
    pub oauth: Mutex<OAuthCache>,
    pub secrets: Mutex<HashMap<String, String>>,
    pub request_values: Mutex<Vec<String>>,
    pub flights: Flights,
}

impl State {
    fn grant_type(&self) -> &str {
        self.cfg
            .grant_type
            .as_deref()
            .filter(|g| !g.is_empty())
            .unwrap_or(
                if self.cfg.username.as_deref().is_some_and(|u| !u.is_empty()) {
                    "password"
                } else {
                    "client_credentials"
                },
            )
    }

    fn oauth(&self) -> std::sync::MutexGuard<'_, OAuthCache> {
        self.oauth.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `collectCredentialValues`.
    pub(crate) fn redactor(&self) -> Redactor {
        let mut r = Redactor::new();
        let cache = self.oauth().clone();
        for v in [
            &self.cfg.password,
            &self.cfg.client_secret,
            &self.basic_auth,
            &cache.token,
            &cache.refresh_token,
        ]
        .into_iter()
        .flatten()
        {
            r.add(v);
        }
        if let (Some(user), Some(pw)) = (&self.cfg.username, &self.cfg.password) {
            let material = format!("{user}:{pw}");
            let encoded = STANDARD.encode(&material);
            r.add(&material);
            r.add(&encoded);
            r.add_authorization_variants("Basic", &encoded);
        }
        if let Some(b) = &self.basic_auth {
            r.add_authorization_variants("Basic", b);
        }
        if let Some(t) = &cache.token {
            r.add_authorization_variants("Bearer", t);
        }
        for v in self
            .secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            r.add(v);
        }
        for v in self
            .request_values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            r.add_authorization_value(v);
        }
        r
    }

    fn note_request_value(&self, value: &str) {
        self.request_values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(value.to_owned());
    }
}

/// `normalizeClientInstanceUrl`.
pub fn normalize_client_instance_url(instance_url: &str) -> Result<String, ClientError> {
    if instance_url.is_empty() {
        return Err(ClientError::plain(
            "ServiceNow instance URL must be a non-empty string",
        ));
    }
    if instance_url.encode_utf16().count() > MAX_URL_LENGTH {
        return Err(ClientError::plain(format!(
            "ServiceNow instance URL must be at most {MAX_URL_LENGTH} characters"
        )));
    }
    let parsed = Url::parse(instance_url)
        .map_err(|_| ClientError::plain("ServiceNow instance URL is not a valid URL"))?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err(ClientError::plain(
            "ServiceNow instance URL must use http or https",
        ));
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || instance_url.contains('?')
        || instance_url.contains('#')
    {
        return Err(ClientError::plain(
            "ServiceNow instance URL must not include credentials, a query or a fragment",
        ));
    }
    Ok(trim_trailing_slashes(instance_url).to_owned())
}

/// `oauthRefreshTokenAccount`: `identity-v1-<sha256 hex>` of the identity tuple.
pub fn oauth_refresh_token_account(
    url: &str,
    client_id: &str,
    authorize_url: Option<&str>,
    token_url: Option<&str>,
    os_user: &str,
) -> Result<String, ClientError> {
    let parsed = Url::parse(url).map_err(|_| ClientError::plain("Invalid URL"))?;
    let instance_url = format!(
        "{}{}",
        parsed.origin().ascii_serialization(),
        trim_trailing_slashes(parsed.path())
    );
    let endpoints = resolve_oauth_endpoints(&instance_url, authorize_url, token_url);
    let href = |u: &str| {
        Url::parse(u)
            .map(|p| p.to_string())
            .map_err(|_| ClientError::plain("Invalid URL"))
    };
    let tuple = serde_json::json!([
        "happy-platform-mcp/oauth-refresh-token",
        1,
        os_user,
        instance_url,
        client_id,
        href(&endpoints.authorize_url)?,
        href(&endpoints.token_url)?
    ]);
    let digest = Sha256::digest(tuple.to_string().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{REFRESH_TOKEN_IDENTITY_PREFIX}{hex}"))
}

/// token-store `isValidTokenAccount`.
pub fn is_valid_token_account(account: &str) -> bool {
    (1..=200).contains(&account.len())
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'@' | b'.' | b'-'))
        && account != "."
        && !account.contains("..")
}

/// `legacyRefreshTokenAccount`.
pub fn legacy_refresh_token_account(instance_name: &str, os_user: &str) -> Option<String> {
    let account = format!("{os_user}@{instance_name}");
    (!instance_name.is_empty() && is_valid_token_account(&account)).then_some(account)
}

fn store_error_code(error: &SecretStoreError) -> &'static str {
    match error {
        SecretStoreError::Unavailable(_) => "KEYCHAIN_UNAVAILABLE",
        SecretStoreError::InvalidAccount => "INVALID_CREDENTIAL_REF",
        _ => "KEYCHAIN_OPERATION_FAILED",
    }
}

/// `clearInstanceRefreshTokens`.
pub async fn clear_instance_refresh_tokens(
    token_store: &dyn SecretStore,
    instance: &InstanceConfig,
    os_user: &str,
) -> Result<(), ClientError> {
    let account = oauth_refresh_token_account(
        &instance.url,
        instance.client_id.as_deref().unwrap_or(""),
        instance.authorize_url.as_deref(),
        instance.token_url.as_deref(),
        os_user,
    )?;
    token_store
        .delete(SERVICE_NAME, &account)
        .await
        .map_err(|e| ClientError::credential(store_error_code(&e)))?;
    if let Some(legacy) = instance
        .name
        .as_deref()
        .and_then(|n| legacy_refresh_token_account(n, os_user))
    {
        token_store
            .delete(SERVICE_NAME, &legacy)
            .await
            .map_err(|e| ClientError::credential(store_error_code(&e)))?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SecretKind {
    Password,
    ClientSecret,
}

/// `credentialRefForType`.
fn credential_ref_for_type(
    reference: Option<&Value>,
    kind: SecretKind,
    expects_object: bool,
) -> Option<String> {
    let reference = reference?;
    if expects_object != reference.is_object() {
        return None;
    }
    let candidate = if let Value::Object(map) = reference {
        if map.keys().any(|k| {
            !["password", "clientSecret", "passwordRef", "clientSecretRef"].contains(&k.as_str())
        }) {
            return None;
        }
        let pick = |a: &str, b: &str| match map.get(a) {
            Some(Value::Null) | None => map.get(b),
            other => other,
        };
        match kind {
            SecretKind::Password => pick("password", "passwordRef"),
            SecretKind::ClientSecret => pick("clientSecret", "clientSecretRef"),
        }?
    } else {
        reference
    };
    let parsed = parse_credential_ref(candidate.as_str()?).ok()?;
    let expected = match kind {
        SecretKind::Password => "password",
        SecretKind::ClientSecret => "client-secret",
    };
    (parsed.kind.as_str() == expected).then_some(parsed.reference)
}

fn validate_token_response(
    data: &Value,
) -> Result<(String, Option<String>, Option<f64>), ClientError> {
    let invalid = |m: &str| ClientError::auth(m, "OAUTH_TOKEN_RESPONSE_INVALID");
    let Value::Object(map) = data else {
        return Err(invalid("OAuth token endpoint returned an invalid response"));
    };
    let access = map
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid("OAuth token endpoint response is missing a valid access token"))?;
    let refresh = match map.get("refresh_token") {
        None => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(_) => {
            return Err(invalid(
                "OAuth token endpoint response contains an invalid refresh token",
            ));
        }
    };
    let expires = match map.get("expires_in") {
        None => None,
        Some(Value::Number(n)) => {
            let v = n.as_f64().unwrap_or(f64::NAN);
            #[allow(clippy::cast_precision_loss)]
            let max = 9_007_199_254_740_991_f64 / 1000.0;
            if !v.is_finite() || v <= 0.0 || v > max {
                return Err(invalid(
                    "OAuth token endpoint response contains an invalid expiry",
                ));
            }
            Some(v)
        }
        Some(_) => {
            return Err(invalid(
                "OAuth token endpoint response contains an invalid expiry",
            ));
        }
    };
    Ok((access.to_owned(), refresh, expires))
}

#[allow(clippy::cast_possible_truncation)]
fn expiry_ms(now: i64, expires_in: Option<f64>) -> i64 {
    now + (expires_in.unwrap_or(1800.0) * 1000.0) as i64
}

/// Table API query parameters, sent in the 6.x order when non-empty.
#[derive(Clone, Debug, Default)]
pub struct RecordQuery {
    pub query: Option<String>,
    pub limit: Option<String>,
    pub fields: Option<String>,
    pub offset: Option<String>,
    pub order_by: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RecordParams {
    pub fields: Option<String>,
    pub display_value: Option<String>,
    pub exclude_reference_link: Option<String>,
}

fn non_empty(v: Option<&String>) -> Option<&str> {
    v.map(String::as_str).filter(|s| !s.is_empty())
}

pub(crate) struct Inner {
    deps: ClientDeps,
    current: RwLock<Arc<State>>,
    progress: Mutex<Option<Arc<dyn Fn(&str, Option<u64>, Option<u64>) + Send + Sync>>>,
}

/// Cheap to clone; clones share one instance state.
#[derive(Clone)]
pub struct ServiceNowClient {
    inner: Arc<Inner>,
}

impl ServiceNowClient {
    pub fn new(config: InstanceConfig, deps: ClientDeps) -> Result<Self, ClientError> {
        let state = Self::build_state(1, config, "default")?;
        Ok(Self {
            inner: Arc::new(Inner {
                deps,
                current: RwLock::new(Arc::new(state)),
                progress: Mutex::new(None),
            }),
        })
    }

    fn build_state(
        generation: u64,
        config: InstanceConfig,
        previous_name: &str,
    ) -> Result<State, ClientError> {
        let url = normalize_client_instance_url(&config.url)?;
        let name = config
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| previous_name.to_owned());
        let basic_auth = (config.auth_type == AuthType::Basic)
            .then(|| {
                config.password.as_ref().map(|pw| {
                    STANDARD.encode(format!(
                        "{}:{pw}",
                        config.username.as_deref().unwrap_or("undefined")
                    ))
                })
            })
            .flatten();
        Ok(State {
            generation,
            url,
            name,
            cfg: config,
            basic_auth,
            oauth: Mutex::new(OAuthCache::default()),
            secrets: Mutex::new(HashMap::new()),
            request_values: Mutex::new(Vec::new()),
            flights: Flights::default(),
        })
    }

    /// `setInstance`: a new generation with fresh caches.
    pub fn set_instance(&self, config: InstanceConfig) -> Result<(), ClientError> {
        let mut current = self
            .inner
            .current
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let state = Self::build_state(current.generation + 1, config, &current.name)?;
        *current = Arc::new(state);
        Ok(())
    }

    pub(crate) fn state(&self) -> Arc<State> {
        self.inner
            .current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn generation(&self) -> u64 {
        self.state().generation
    }

    /// Drops the cached access token of `generation` (after a 401). Returns
    /// false when the generation has moved.
    pub fn invalidate_access_token(&self, generation: u64) -> bool {
        let st = self.state();
        if st.generation != generation {
            return false;
        }
        let mut cache = st.oauth();
        cache.token = None;
        cache.expiry = None;
        true
    }

    pub(crate) fn assert_current(&self, st: &State) -> Result<(), ClientError> {
        if self.generation() == st.generation {
            Ok(())
        } else {
            Err(ClientError::stale())
        }
    }

    pub(crate) fn deps(&self) -> &ClientDeps {
        &self.inner.deps
    }

    pub fn set_progress_callback(
        &self,
        callback: Option<Arc<dyn Fn(&str, Option<u64>, Option<u64>) + Send + Sync>>,
    ) {
        *self
            .inner
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = callback;
    }

    pub fn notify_progress(&self, message: &str, current: Option<u64>, total: Option<u64>) {
        let cb = self
            .inner
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(cb) = cb {
            cb(message, current, total);
        }
    }

    /// `getCurrentInstance`.
    pub fn current_instance(&self) -> Value {
        let st = self.state();
        serde_json::json!({"name": st.name, "url": st.url, "authType": st.cfg.auth_type.as_str()})
    }

    pub fn instance_url(&self) -> String {
        self.state().url.clone()
    }

    pub fn auth_type(&self) -> AuthType {
        self.state().cfg.auth_type
    }

    fn token_poster(&self) -> Arc<dyn TokenPoster> {
        self.inner
            .deps
            .token_poster
            .clone()
            .unwrap_or_else(|| Arc::new(HttpTokenPoster::new(self.inner.deps.transport.clone())))
    }

    // ---- credentials -------------------------------------------------------

    /// `_resolveCredential`.
    async fn resolve_credential(
        &self,
        st: &Arc<State>,
        kind: SecretKind,
    ) -> Result<String, ClientError> {
        self.assert_current(st)?;
        let legacy = match kind {
            SecretKind::Password => &st.cfg.password,
            SecretKind::ClientSecret => &st.cfg.client_secret,
        };
        if let Some(value) = legacy {
            return Ok(value.clone());
        }
        let grant = st.grant_type();
        let oauth_password = st.cfg.auth_type == AuthType::OAuth && grant == "password";
        let reference = st.cfg.credential_ref.as_ref();
        let shape_ok = if oauth_password {
            credential_ref_for_type(reference, SecretKind::Password, true).is_some()
                && credential_ref_for_type(reference, SecretKind::ClientSecret, true).is_some()
        } else {
            let k = if st.cfg.auth_type == AuthType::Basic {
                SecretKind::Password
            } else {
                SecretKind::ClientSecret
            };
            credential_ref_for_type(reference, k, false).is_some()
        };
        if !shape_ok {
            return Err(ClientError::credential("CREDENTIAL_NOT_FOUND"));
        }
        let Some(cref) = credential_ref_for_type(reference, kind, oauth_password) else {
            return Err(ClientError::credential("CREDENTIAL_NOT_FOUND"));
        };
        let key = format!(
            "{}:{cref}",
            if kind == SecretKind::Password {
                "password"
            } else {
                "clientSecret"
            }
        );
        if let Some(v) = st
            .secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(v.clone());
        }
        let store = self.inner.deps.credential_store.clone();
        let st2 = st.clone();
        let flight_key = format!("cred:{key}");
        st.flights
            .run(&flight_key, || async move {
                let result = store.get_secret(&cref).await;
                self.assert_current(&st2)?;
                let value = result.map_err(|e| ClientError::credential(e.code()))?;
                let value = value.expose().to_owned();
                if value.is_empty() {
                    return Err(ClientError::credential("CREDENTIAL_NOT_FOUND"));
                }
                st2.secrets
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, value.clone());
                Ok(value)
            })
            .await
    }

    /// `getAuthHeader` for the current generation.
    pub async fn auth_header(&self) -> Result<String, ClientError> {
        let st = self.state();
        self.auth_header_for(&st).await
    }

    pub(crate) async fn auth_header_for(&self, st: &Arc<State>) -> Result<String, ClientError> {
        self.assert_current(st)?;
        if st.cfg.auth_type == AuthType::OAuth {
            let token = self.oauth_token(st).await?;
            self.assert_current(st)?;
            return Ok(format!("Bearer {token}"));
        }
        if let Some(b) = &st.basic_auth {
            return Ok(format!("Basic {b}"));
        }
        let password = self.resolve_credential(st, SecretKind::Password).await?;
        self.assert_current(st)?;
        Ok(format!(
            "Basic {}",
            STANDARD.encode(format!(
                "{}:{password}",
                st.cfg.username.as_deref().unwrap_or("undefined")
            ))
        ))
    }

    // ---- OAuth -------------------------------------------------------------

    fn cached_token(&self, st: &State) -> Option<String> {
        let cache = st.oauth();
        match (&cache.token, cache.expiry) {
            (Some(t), Some(exp))
                if !t.is_empty() && self.inner.deps.clock.now_ms() < exp - 30_000 =>
            {
                Some(t.clone())
            }
            _ => None,
        }
    }

    /// `_getOAuthToken`.
    pub(crate) async fn oauth_token(&self, st: &Arc<State>) -> Result<String, ClientError> {
        self.assert_current(st)?;
        if let Some(t) = self.cached_token(st) {
            return Ok(t);
        }
        let token = st
            .flights
            .run("oauth-token", || self.request_oauth_token(st))
            .await?;
        self.assert_current(st)?;
        Ok(token)
    }

    fn approved_endpoint(&self, st: &State, field: &str) -> Result<String, ClientError> {
        let endpoints = resolve_oauth_endpoints(
            &st.url,
            st.cfg.authorize_url.as_deref(),
            st.cfg.token_url.as_deref(),
        );
        let endpoint = if field == "authorizeUrl" {
            endpoints.authorize_url
        } else {
            endpoints.token_url
        };
        let trusted = (self.inner.deps.trusted_origins)();
        assert_approved_oauth_endpoint(&endpoint, &st.url, field, trusted.as_deref())
    }

    fn safe_auth(&self, st: &State, raw: RawError, default_code: Option<&str>) -> ClientError {
        let mut safe = raw.into_safe_auth(st.redactor());
        if safe.code.is_none() {
            safe.code = default_code.map(str::to_owned);
        }
        safe
    }

    async fn request_oauth_token(&self, st: &Arc<State>) -> Result<String, ClientError> {
        if st.cfg.grant_type.as_deref() == Some("authorization_code") {
            return self.authorization_code_token(st).await;
        }
        self.assert_current(st)?;
        self.approved_endpoint(st, "tokenUrl")?;
        let client_secret = self
            .resolve_credential(st, SecretKind::ClientSecret)
            .await?;
        self.assert_current(st)?;
        let client_id = st.cfg.client_id.clone().unwrap_or_default();
        let poster = self.token_poster();

        let refresh = st.oauth().refresh_token.clone();
        if let Some(refresh) = refresh {
            let url = self.approved_endpoint(st, "tokenUrl")?;
            let params = vec![
                ("grant_type".to_owned(), "refresh_token".to_owned()),
                ("client_id".to_owned(), client_id.clone()),
                ("client_secret".to_owned(), client_secret.clone()),
                ("refresh_token".to_owned(), refresh),
            ];
            let result = poster.post(&url, params).await;
            self.assert_current(st)?;
            match result {
                Ok(data) => {
                    return self.handle_token_response(st, &data).map_err(|e| {
                        self.safe_auth(
                            st,
                            RawError::from_client(&e),
                            Some("OAUTH_REFRESH_REQUEST_FAILED"),
                        )
                    });
                }
                Err(raw) if !raw.is_explicit_invalid_grant() => {
                    return Err(self.safe_auth(st, raw, Some("OAUTH_REFRESH_REQUEST_FAILED")));
                }
                Err(_) => {
                    eprintln!(
                        "OAuth refresh token rejected; requesting the configured primary grant"
                    );
                    st.oauth().refresh_token = None;
                }
            }
        }

        let grant = st.grant_type().to_owned();
        let mut params = vec![
            ("grant_type".to_owned(), grant.clone()),
            ("client_id".to_owned(), client_id),
            ("client_secret".to_owned(), client_secret),
        ];
        if grant == "password" {
            params.push((
                "username".to_owned(),
                st.cfg
                    .username
                    .clone()
                    .unwrap_or_else(|| "undefined".to_owned()),
            ));
            let password = self.resolve_credential(st, SecretKind::Password).await?;
            params.push(("password".to_owned(), password));
        }
        if let Some(scope) = st.cfg.scope.as_deref().filter(|s| !s.is_empty()) {
            params.push(("scope".to_owned(), scope.to_owned()));
        }
        self.assert_current(st)?;
        let url = self.approved_endpoint(st, "tokenUrl")?;
        let result = poster.post(&url, params).await;
        self.assert_current(st)?;
        match result {
            Ok(data) => self.handle_token_response(st, &data).map_err(|e| {
                self.safe_auth(
                    st,
                    RawError::from_client(&e),
                    Some("OAUTH_TOKEN_REQUEST_FAILED"),
                )
            }),
            Err(raw) => Err(self.safe_auth(st, raw, Some("OAUTH_TOKEN_REQUEST_FAILED"))),
        }
    }

    /// `_handleTokenResponse`.
    fn handle_token_response(&self, st: &State, data: &Value) -> Result<String, ClientError> {
        self.assert_current(st)?;
        let (access, refresh, expires) = validate_token_response(data)?;
        let mut cache = st.oauth();
        cache.token = Some(access.clone());
        if refresh.is_some() {
            cache.refresh_token = refresh;
        }
        cache.expiry = Some(expiry_ms(self.inner.deps.clock.now_ms(), expires));
        Ok(access)
    }

    async fn token_store_get(&self, account: &str) -> Result<Option<String>, ClientError> {
        self.inner
            .deps
            .token_store
            .get(SERVICE_NAME, account)
            .await
            .map(|v| v.map(|s| s.expose().to_owned()))
            .map_err(|e| ClientError::credential(store_error_code(&e)))
    }

    async fn token_store_clear(&self, account: &str) -> Result<(), ClientError> {
        self.inner
            .deps
            .token_store
            .delete(SERVICE_NAME, account)
            .await
            .map(|_| ())
            .map_err(|e| ClientError::credential(store_error_code(&e)))
    }

    /// `_getAuthorizationCodeToken`.
    async fn authorization_code_token(&self, st: &Arc<State>) -> Result<String, ClientError> {
        self.assert_current(st)?;
        if let Some(t) = self.cached_token(st) {
            return Ok(t);
        }
        let authorize_url = self.approved_endpoint(st, "authorizeUrl")?;
        let token_url = self.approved_endpoint(st, "tokenUrl")?;
        let client_id = st.cfg.client_id.clone().unwrap_or_default();
        let os_user = self.inner.deps.os_user.clone();
        let account = oauth_refresh_token_account(
            &st.url,
            &client_id,
            Some(&authorize_url),
            Some(&token_url),
            &os_user,
        )?;
        let legacy = legacy_refresh_token_account(&st.name, &os_user);

        if st.oauth().refresh_token.is_none() {
            let stored = self.token_store_get(&account).await;
            self.assert_current(st)?;
            st.oauth().refresh_token = stored?;
        }

        let refresh = st.oauth().refresh_token.clone();
        if let Some(refresh) = refresh {
            let mut params = vec![
                ("grant_type".to_owned(), "refresh_token".to_owned()),
                ("client_id".to_owned(), client_id.clone()),
                ("refresh_token".to_owned(), refresh),
            ];
            if let Some(secret) = st.cfg.client_secret.as_deref().filter(|s| !s.is_empty()) {
                params.push(("client_secret".to_owned(), secret.to_owned()));
            }
            self.assert_current(st)?;
            let url = self.approved_endpoint(st, "tokenUrl")?;
            let result = self.token_poster().post(&url, params).await;
            self.assert_current(st)?;
            match result {
                Ok(data) => {
                    return match self.accept_auth_code_tokens(st, &data, &account).await {
                        Ok(t) => Ok(t),
                        Err(e) if e.is_stale() || e.name == "CredentialResolutionError" => Err(e),
                        Err(e) => Err(self.safe_auth(st, RawError::from_client(&e), None)),
                    };
                }
                Err(raw) => {
                    if !matches!(raw.status, Some(400 | 401)) || !raw.is_explicit_invalid_grant() {
                        return Err(self.safe_auth(st, raw, None));
                    }
                    eprintln!(
                        "OAuth refresh token rejected; re-authenticating via browser sign-in"
                    );
                    st.oauth().refresh_token = None;
                    let cleared = self.token_store_clear(&account).await;
                    self.assert_current(st)?;
                    cleared?;
                }
            }
        }

        let mut flow = AuthCodeFlowConfig::new(&authorize_url, &token_url, &client_id);
        flow.client_secret = st.cfg.client_secret.clone();
        flow.scope = st.cfg.scope.clone();
        if let Some(port) = st.cfg.redirect_port {
            flow.redirect_port = port;
        }
        if let Some(path) = st.cfg.callback_path.clone().filter(|p| !p.is_empty()) {
            flow.callback_path = path;
        }
        let approved: Arc<dyn TokenPoster> = Arc::new(ApprovedExchange {
            client: self.clone(),
            state: st.clone(),
            token_url: token_url.clone(),
            inner: self.token_poster(),
        });
        self.assert_current(st)?;
        let result = self.inner.deps.auth_flow.run(flow, approved).await;
        self.assert_current(st)?;
        let data = match result {
            Ok(data) => data,
            Err(raw) if raw.code.as_deref() == Some(error::STALE_INSTANCE_CODE) => {
                return Err(ClientError::stale());
            }
            Err(raw) => return Err(self.safe_auth(st, raw, None)),
        };
        let token = match self.accept_auth_code_tokens(st, &data, &account).await {
            Ok(t) => t,
            Err(e) if e.is_stale() || e.name == "CredentialResolutionError" => return Err(e),
            Err(e) => return Err(self.safe_auth(st, RawError::from_client(&e), None)),
        };
        if let Some(legacy) = legacy {
            if self.token_store_clear(&legacy).await.is_err() {
                eprintln!(
                    "Could not remove a legacy name-keyed OAuth refresh token entry; remove it from the token store manually"
                );
            }
        }
        Ok(token)
    }

    /// `_acceptAuthCodeTokens`: persist a changed refresh token before
    /// publishing any response-derived cache state.
    async fn accept_auth_code_tokens(
        &self,
        st: &Arc<State>,
        data: &Value,
        account: &str,
    ) -> Result<String, ClientError> {
        self.assert_current(st)?;
        let (access, refresh, expires) = validate_token_response(data)?;
        let previous = st.oauth().clone();
        if let Some(refresh) = &refresh {
            let persisted: Result<(), ClientError> = async {
                let stored = self.token_store_get(account).await;
                self.assert_current(st)?;
                if stored?.as_deref() != Some(refresh.as_str()) {
                    let set = self
                        .inner
                        .deps
                        .token_store
                        .set(
                            SERVICE_NAME,
                            account,
                            hpm_types::credential::Secret::new(refresh.clone()),
                        )
                        .await;
                    self.assert_current(st)?;
                    set.map_err(|e| ClientError::credential(store_error_code(&e)))?;
                }
                Ok(())
            }
            .await;
            if let Err(error) = persisted {
                if error.is_stale() {
                    return Err(error);
                }
                self.assert_current(st)?;
                let still_valid = matches!(
                    (&previous.token, previous.expiry),
                    (Some(t), Some(exp)) if !t.is_empty() && self.inner.deps.clock.now_ms() < exp - 30_000
                );
                *st.oauth() = if still_valid {
                    previous
                } else {
                    OAuthCache::default()
                };
                return Err(error);
            }
        }
        self.assert_current(st)?;
        let mut cache = st.oauth();
        cache.token = Some(access.clone());
        if refresh.is_some() {
            cache.refresh_token = refresh;
        }
        cache.expiry = Some(expiry_ms(self.inner.deps.clock.now_ms(), expires));
        Ok(access)
    }

    // ---- REST ---------------------------------------------------------------

    pub(crate) fn sanitize(&self, st: &State, raw: RawError) -> ClientError {
        raw.into_safe_servicenow(st.redactor())
    }

    /// One Table/REST API call against the current generation, with OAuth
    /// refresh-and-retry-once on 401. Returns the parsed body.
    pub async fn rest(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ClientError> {
        let st = self.state();
        let header = self.auth_header_for(&st).await?;
        self.assert_current(&st)?;
        st.note_request_value(&header);
        let build = |auth: &str| {
            let mut req = HttpRequest::new(method, format!("{}{path}", st.url));
            req.set_header("Accept", "application/json");
            req.set_header("Content-Type", "application/json");
            req.set_header("User-Agent", REST_USER_AGENT);
            req.set_header("Authorization", auth);
            req.body = body.map(|b| b.to_string().into_bytes());
            req
        };
        let mut response = self.send(&st, build(&header)).await?;
        if response.status == 401 && st.cfg.auth_type == AuthType::OAuth {
            {
                let mut cache = st.oauth();
                cache.token = None;
                cache.expiry = None;
            }
            let token = self.oauth_token(&st).await?;
            self.assert_current(&st)?;
            let retry = format!("Bearer {token}");
            st.note_request_value(&retry);
            response = self.send(&st, build(&retry)).await?;
        }
        if (200..300).contains(&response.status) {
            Ok(response.data())
        } else {
            Err(self.sanitize(&st, RawError::http(response.status, response.data())))
        }
    }

    pub(crate) async fn send(
        &self,
        st: &State,
        request: HttpRequest,
    ) -> Result<HttpResponse, ClientError> {
        let result = self.inner.deps.transport.send(request).await;
        self.assert_current(st)?;
        result.map_err(|e| {
            self.sanitize(
                st,
                RawError {
                    message: e.message,
                    code: e.code,
                    ..RawError::default()
                },
            )
        })
    }

    fn result_of(data: Value) -> Value {
        match data {
            Value::Object(mut m) => m.remove("result").unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }

    /// `getRecords`.
    pub async fn get_records(
        &self,
        table: &str,
        query: &RecordQuery,
    ) -> Result<Value, ClientError> {
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        for (name, value) in [
            ("sysparm_query", &query.query),
            ("sysparm_limit", &query.limit),
            ("sysparm_fields", &query.fields),
            ("sysparm_offset", &query.offset),
            ("sysparm_order_by", &query.order_by),
        ] {
            if let Some(v) = non_empty(value.as_ref()) {
                pairs.push((name, v));
            }
        }
        let path = format!("/api/now/table/{table}?{}", form_encode(&pairs));
        Ok(Self::result_of(self.rest(Method::Get, &path, None).await?))
    }

    /// `getRecord`.
    pub async fn get_record(
        &self,
        table: &str,
        sys_id: &str,
        params: &RecordParams,
    ) -> Result<Value, ClientError> {
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        for (name, value) in [
            ("sysparm_fields", &params.fields),
            ("sysparm_display_value", &params.display_value),
            (
                "sysparm_exclude_reference_link",
                &params.exclude_reference_link,
            ),
        ] {
            if let Some(v) = non_empty(value.as_ref()) {
                pairs.push((name, v));
            }
        }
        let qs = form_encode(&pairs);
        let path = if qs.is_empty() {
            format!("/api/now/table/{table}/{sys_id}")
        } else {
            format!("/api/now/table/{table}/{sys_id}?{qs}")
        };
        Ok(Self::result_of(self.rest(Method::Get, &path, None).await?))
    }

    pub async fn create_record(&self, table: &str, data: &Value) -> Result<Value, ClientError> {
        Ok(Self::result_of(
            self.rest(Method::Post, &format!("/api/now/table/{table}"), Some(data))
                .await?,
        ))
    }

    pub async fn update_record(
        &self,
        table: &str,
        sys_id: &str,
        data: &Value,
    ) -> Result<Value, ClientError> {
        Ok(Self::result_of(
            self.rest(
                Method::Put,
                &format!("/api/now/table/{table}/{sys_id}"),
                Some(data),
            )
            .await?,
        ))
    }

    pub async fn delete_record(&self, table: &str, sys_id: &str) -> Result<Value, ClientError> {
        self.rest(
            Method::Delete,
            &format!("/api/now/table/{table}/{sys_id}"),
            None,
        )
        .await?;
        Ok(serde_json::json!({"success": true}))
    }

    pub(crate) async fn sleep_ms(&self, ms: u64) {
        self.inner.deps.clock.sleep(ms).await;
    }

    pub(crate) fn now_ms(&self) -> i64 {
        self.inner.deps.clock.now_ms()
    }
}

/// The code exchange re-checks generation and endpoint approval right before
/// posting (`approvedExchange`).
struct ApprovedExchange {
    client: ServiceNowClient,
    state: Arc<State>,
    token_url: String,
    inner: Arc<dyn TokenPoster>,
}

impl TokenPoster for ApprovedExchange {
    fn post<'a>(
        &'a self,
        url: &'a str,
        params: Vec<(String, String)>,
    ) -> BoxFut<'a, Result<Value, RawError>> {
        Box::pin(async move {
            if self.client.assert_current(&self.state).is_err() {
                return Err(RawError::from_client(&ClientError::stale()));
            }
            let current = self.client.approved_endpoint(&self.state, "tokenUrl").ok();
            if url != self.token_url || current.as_deref() != Some(self.token_url.as_str()) {
                return Err(RawError::from_client(&ClientError::auth(
                    "OAuth token endpoint changed during sign-in",
                    "OAUTH_ENDPOINT_NOT_APPROVED",
                )));
            }
            self.inner.post(&self.token_url, params).await
        })
    }
}

/// Request timeout helper for UI sessions.
pub(crate) const UI_TIMEOUT: Duration = Duration::from_millis(30_000);
