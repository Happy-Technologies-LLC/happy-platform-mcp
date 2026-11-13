//! [`PinnedClient`]: a client handle pinned to one instance generation
//! (W3 baseline G2, W2-B L-C). Every request runs on the pinned generation's
//! URL and credentials and checks that it is still the live one before any
//! credential is built and after every await, so a `set_instance`
//! (reconfiguration) between two steps of one call fails the later step closed
//! instead of sending it to another instance. [`PinnedClient::client`] hands out
//! a pinned [`ServiceNowClient`] clone, so multi-step flows (scripts, triggers,
//! pickers, ATF) get the same guarantee with no check-then-act.

use std::sync::Arc;

use serde_json::Value;

use super::http::Method;
use super::{
    ClientError, RecordParams, RecordQuery, ServiceNowClient, State, record_path, records_path,
    result_of, table_path,
};

/// Cheap to clone. Obtain with [`ServiceNowClient::pinned`] or, in tools,
/// through `hpm_types::servicenow::ServiceNowAccess`.
#[derive(Clone)]
pub struct PinnedClient {
    client: ServiceNowClient,
    state: Arc<State>,
}

impl PinnedClient {
    pub(crate) fn new(client: &ServiceNowClient, state: Arc<State>) -> Self {
        Self {
            client: client.pinned_to(state.clone()),
            state,
        }
    }

    pub fn generation(&self) -> u64 {
        self.state.generation
    }

    /// The pinned instance's URL (not the client's current one).
    pub fn instance_url(&self) -> &str {
        &self.state.url
    }

    /// The pinned instance's registry name.
    pub fn instance_name(&self) -> &str {
        &self.state.name
    }

    /// Stale (`ClientError::is_stale`) once the pinned generation moved.
    pub fn assert_current(&self) -> Result<(), ClientError> {
        self.client.assert_current(&self.state)
    }

    /// The [`ServiceNowClient`] pinned to this generation, for multi-step flows
    /// (`execute_script_via_processor`/`_via_trigger`, pickers, ATF): every
    /// method runs on the pinned instance only and fails stale once it moved.
    /// `set_instance` on it is refused.
    pub fn client(&self) -> &ServiceNowClient {
        &self.client
    }

    pub async fn rest(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ClientError> {
        Ok(self.rest_with_status(method, path, body).await?.1)
    }

    pub async fn rest_with_status(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), ClientError> {
        self.client.rest_in(&self.state, method, path, body).await
    }

    /// `getRecords` on the pinned instance.
    pub async fn get_records(
        &self,
        table: &str,
        query: &RecordQuery,
    ) -> Result<Value, ClientError> {
        let path = records_path(table, query)?;
        Ok(result_of(self.rest(Method::Get, &path, None).await?))
    }

    /// `getRecord` on the pinned instance.
    pub async fn get_record(
        &self,
        table: &str,
        sys_id: &str,
        params: &RecordParams,
    ) -> Result<Value, ClientError> {
        let path = record_path(table, sys_id, params)?;
        Ok(result_of(self.rest(Method::Get, &path, None).await?))
    }

    pub async fn create_record(&self, table: &str, data: &Value) -> Result<Value, ClientError> {
        let path = table_path(table, None)?;
        Ok(result_of(self.rest(Method::Post, &path, Some(data)).await?))
    }

    pub async fn update_record(
        &self,
        table: &str,
        sys_id: &str,
        data: &Value,
    ) -> Result<Value, ClientError> {
        let path = table_path(table, Some(sys_id))?;
        Ok(result_of(self.rest(Method::Put, &path, Some(data)).await?))
    }
}
