//! Single-user [`CredentialProvider`] and [`ServiceNowAccess`] over one
//! [`ServiceNowClient`] per registry instance ([`InstanceClients`], 6.2.0
//! `resolveClient`). A binding names the instance it was prepared on and that
//! client's generation; a `set_instance` (reconfiguration) between prepare and
//! any later step fails the call closed, and a call can never reach another
//! instance's client.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use hpm_types::BoxFut;
use hpm_types::context::{RequestContext, ResolvedInstance};
use hpm_types::credential::{
    Credential, CredentialBinding, CredentialError, CredentialKind, CredentialNeed,
    CredentialProvider, Secret, UiSession, UiSessionScope,
};
use hpm_types::servicenow::ServiceNowAccess;
use hpm_types::tool::ToolError;
use serde_json::{Map, Value};

use crate::client::{
    AuthType, ClientDeps, ClientError, InstanceConfig, PinnedClient, ServiceNowClient,
};
use crate::config::ConfigManager;

type Object = Map<String, Value>;

/// Looks up the raw registry entry of an instance by id (single-user: its name).
pub type InstanceResolver = Arc<dyn Fn(&str) -> Result<Object, ClientError> + Send + Sync>;

fn str_field(o: &Object, key: &str) -> Option<String> {
    o.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `createServiceNowClient(instance)`: url, username, password and
/// `instanceToClientOptions`.
pub fn instance_config_from_registry(o: &Object) -> InstanceConfig {
    let redirect_port = match o.get("redirectPort") {
        Some(Value::Number(n)) => n.as_u64().and_then(|p| u16::try_from(p).ok()),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    };
    InstanceConfig {
        url: str_field(o, "url").unwrap_or_default(),
        name: str_field(o, "name"),
        username: str_field(o, "username"),
        password: str_field(o, "password"),
        auth_type: if o.get("authType").and_then(Value::as_str) == Some("oauth") {
            AuthType::OAuth
        } else {
            AuthType::Basic
        },
        client_id: str_field(o, "clientId"),
        client_secret: str_field(o, "clientSecret"),
        grant_type: str_field(o, "grantType"),
        scope: str_field(o, "scope"),
        authorize_url: str_field(o, "authorizeUrl"),
        token_url: str_field(o, "tokenUrl"),
        redirect_port,
        callback_path: str_field(o, "callbackPath"),
        credential_ref: o.get("credentialRef").cloned(),
    }
}

struct Entry {
    config_version: u64,
    /// Registry entry the client was configured from.
    source: Object,
    client: ServiceNowClient,
}

/// One client per registry instance, created on first use (6.2.0 caches a client
/// per instance name). A new `config_version` whose registry entry changed
/// re-applies it with `set_instance`, which starts a new generation.
pub struct InstanceClients {
    deps: ClientDeps,
    resolve: InstanceResolver,
    clients: Mutex<HashMap<String, Entry>>,
}

impl InstanceClients {
    pub fn new(deps: ClientDeps, resolve: InstanceResolver) -> Self {
        Self {
            deps,
            resolve,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Resolves instances from the config manager's registry (`getInstance`).
    pub fn from_config(config: Arc<ConfigManager>, deps: ClientDeps) -> Self {
        Self::new(
            deps,
            Arc::new(move |id| {
                config
                    .get_instance(id)
                    .map_err(|e| ClientError::plain(e.to_string()))
            }),
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The client of `instance`, configured for its `config_version`.
    pub fn client_for(&self, instance: &ResolvedInstance) -> Result<ServiceNowClient, ClientError> {
        let mut clients = self.lock();
        if let Some(e) = clients.get(&instance.instance_id)
            && e.config_version == instance.config_version
        {
            return Ok(e.client.clone());
        }
        let source = (self.resolve)(&instance.instance_id)?;
        match clients.get_mut(&instance.instance_id) {
            Some(e) => {
                if e.source != source {
                    e.client
                        .set_instance(instance_config_from_registry(&source))?;
                    e.source = source;
                }
                e.config_version = instance.config_version;
                Ok(e.client.clone())
            }
            None => {
                let client = ServiceNowClient::new(
                    instance_config_from_registry(&source),
                    self.deps.clone(),
                )?;
                clients.insert(
                    instance.instance_id.clone(),
                    Entry {
                        config_version: instance.config_version,
                        source,
                        client: client.clone(),
                    },
                );
                Ok(client)
            }
        }
    }

    /// The client a binding was prepared on. `None` only if it was never created.
    fn bound(&self, instance_id: &str) -> Option<ServiceNowClient> {
        self.lock().get(instance_id).map(|e| e.client.clone())
    }

    /// The client of `binding`, pinned to its generation.
    pub fn pinned(&self, binding: &CredentialBinding) -> Result<PinnedClient, CredentialError> {
        self.bound(binding.instance_id())
            .ok_or(CredentialError::GenerationMoved)?
            .pinned(binding.generation())
            .map_err(|_| CredentialError::GenerationMoved)
    }
}

impl ServiceNowAccess for InstanceClients {
    type Client = PinnedClient;

    fn client<'a>(
        &'a self,
        ctx: &'a RequestContext,
    ) -> BoxFut<'a, Result<PinnedClient, ToolError>> {
        Box::pin(async move {
            let binding = ctx.credential().ok_or(CredentialError::NoInstance)?;
            Ok(self.pinned(binding)?)
        })
    }
}

pub struct SingleUserCredentialProvider {
    clients: Arc<InstanceClients>,
}

impl SingleUserCredentialProvider {
    pub fn new(clients: Arc<InstanceClients>) -> Self {
        Self { clients }
    }

    fn client(&self, instance_id: &str) -> Result<ServiceNowClient, CredentialError> {
        self.clients
            .bound(instance_id)
            .ok_or(CredentialError::GenerationMoved)
    }

    fn check(
        client: &ServiceNowClient,
        binding: &CredentialBinding,
    ) -> Result<(), CredentialError> {
        if binding.generation() == client.generation() {
            Ok(())
        } else {
            Err(CredentialError::GenerationMoved)
        }
    }
}

fn backend(error: ClientError) -> CredentialError {
    if error.is_stale() {
        CredentialError::GenerationMoved
    } else {
        CredentialError::Backend(Box::new(error))
    }
}

/// `Url::origin()` (scheme, lowercased host, default-normalised port) plus the
/// path without trailing slashes: two spellings of one instance URL compare equal.
fn canonical_instance_url(raw: &str) -> Option<(url::Origin, String)> {
    let url = url::Url::parse(raw).ok()?;
    let origin = url.origin();
    // Opaque origins (non-special schemes) are unique per parse: reject them.
    if !origin.is_tuple() {
        return None;
    }
    Some((origin, url.path().trim_end_matches('/').to_owned()))
}

/// Fails closed: an unparsable URL on either side never matches.
pub(crate) fn same_instance_url(a: &str, b: &str) -> bool {
    match (canonical_instance_url(a), canonical_instance_url(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

impl CredentialProvider for SingleUserCredentialProvider {
    fn prepare<'a>(
        &'a self,
        ctx: &'a RequestContext,
        need: CredentialNeed,
    ) -> BoxFut<'a, Result<Option<CredentialBinding>, CredentialError>> {
        Box::pin(async move {
            if need == CredentialNeed::None {
                return Ok(None);
            }
            let instance = ctx.instance().ok_or(CredentialError::NoInstance)?;
            let client = self.clients.client_for(instance).map_err(backend)?;
            let generation = client.generation();
            // Completes any interactive sign-in or refresh before admission.
            client.auth_header().await.map_err(backend)?;
            if client.generation() != generation {
                return Err(CredentialError::GenerationMoved);
            }
            // The binding must name the instance the client actually targets.
            if !same_instance_url(&instance.canonical_url, &client.instance_url()) {
                return Err(CredentialError::GenerationMoved);
            }
            let name = client.current_instance()["name"]
                .as_str()
                .unwrap_or("default")
                .to_owned();
            Ok(Some(CredentialBinding::new(
                ctx.principal().clone(),
                instance.instance_id.clone(),
                instance.config_version,
                generation,
                None,
                name,
            )))
        })
    }

    fn authorize<'a>(
        &'a self,
        binding: &'a CredentialBinding,
    ) -> BoxFut<'a, Result<Credential, CredentialError>> {
        Box::pin(async move {
            let client = self.client(binding.instance_id())?;
            Self::check(&client, binding)?;
            // Never refreshes or prompts: `prepare` already did that.
            let header = client
                .prepared_auth_header(binding.generation())
                .await
                .map_err(backend)?
                .ok_or(CredentialError::GenerationMoved)?;
            Self::check(&client, binding)?;
            Ok(Credential {
                kind: CredentialKind::Authorization(Secret::new(header)),
                generation: binding.generation(),
                expires_at: None,
            })
        })
    }

    fn invalidate<'a>(
        &'a self,
        binding: &'a CredentialBinding,
    ) -> BoxFut<'a, Result<(), CredentialError>> {
        Box::pin(async move {
            if self
                .client(binding.instance_id())?
                .invalidate_access_token(binding.generation())
            {
                Ok(())
            } else {
                Err(CredentialError::GenerationMoved)
            }
        })
    }

    fn ui_session<'a>(
        &'a self,
        binding: &'a CredentialBinding,
        scope: UiSessionScope,
    ) -> BoxFut<'a, Result<UiSession, CredentialError>> {
        Box::pin(async move {
            let client = self.client(binding.instance_id())?;
            Self::check(&client, binding)?;
            let (cookie, csrf, generation) = client.open_ui_session().await.map_err(backend)?;
            if generation != binding.generation() {
                return Err(CredentialError::GenerationMoved);
            }
            Ok(UiSession {
                cookie_header: Secret::new(cookie),
                csrf_token: Secret::new(csrf),
                instance_id: binding.instance_id().to_owned(),
                generation,
                scope,
            })
        })
    }

    fn end_ui_session<'a>(&'a self, session: UiSession) -> BoxFut<'a, Result<(), CredentialError>> {
        Box::pin(async move {
            let client = self.client(&session.instance_id)?;
            if session.generation != client.generation() {
                return Err(CredentialError::GenerationMoved);
            }
            client
                .logout_ui_session(session.cookie_header.expose())
                .await
                .map_err(backend)
        })
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
