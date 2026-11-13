//! Single-user [`CredentialProvider`] over one [`ServiceNowClient`]: the
//! binding's generation is the client's instance generation, so a
//! `set_instance` between prepare and authorize fails the call closed.

use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::credential::{
    Credential, CredentialBinding, CredentialError, CredentialKind, CredentialNeed,
    CredentialProvider, Secret, UiSession, UiSessionScope,
};

use crate::client::{ClientError, ServiceNowClient};

pub struct SingleUserCredentialProvider {
    client: ServiceNowClient,
}

impl SingleUserCredentialProvider {
    pub fn new(client: ServiceNowClient) -> Self {
        Self { client }
    }
}

fn backend(error: ClientError) -> CredentialError {
    if error.is_stale() {
        CredentialError::GenerationMoved
    } else {
        CredentialError::Backend(Box::new(error))
    }
}

impl SingleUserCredentialProvider {
    fn check(&self, binding: &CredentialBinding) -> Result<(), CredentialError> {
        if binding.generation() == self.client.generation() {
            Ok(())
        } else {
            Err(CredentialError::GenerationMoved)
        }
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
            let generation = self.client.generation();
            // Completes any interactive sign-in or refresh before admission.
            self.client.auth_header().await.map_err(backend)?;
            if self.client.generation() != generation {
                return Err(CredentialError::GenerationMoved);
            }
            let current = self.client.current_instance();
            let name = current["name"].as_str().unwrap_or("default").to_owned();
            let (instance_id, config_version) = ctx.instance().map_or_else(
                || (name.clone(), 0),
                |i| (i.instance_id.clone(), i.config_version),
            );
            Ok(Some(CredentialBinding::new(
                ctx.principal().clone(),
                instance_id,
                config_version,
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
            self.check(binding)?;
            let header = self.client.auth_header().await.map_err(backend)?;
            self.check(binding)?;
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
            if self.client.invalidate_access_token(binding.generation()) {
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
            self.check(binding)?;
            let (cookie, csrf, generation) =
                self.client.open_ui_session().await.map_err(backend)?;
            if generation != binding.generation() {
                return Err(CredentialError::GenerationMoved);
            }
            Ok(UiSession {
                cookie_header: Secret::new(cookie),
                csrf_token: Secret::new(csrf),
                generation,
                scope,
            })
        })
    }

    fn end_ui_session<'a>(&'a self, session: UiSession) -> BoxFut<'a, Result<(), CredentialError>> {
        Box::pin(async move {
            if session.generation != self.client.generation() {
                return Err(CredentialError::GenerationMoved);
            }
            self.client
                .logout_ui_session(session.cookie_header.expose())
                .await
                .map_err(backend)
        })
    }
}
